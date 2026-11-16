// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Input lab: the Windows Deck's input adapter in a plain window, driven by
//! injected keys, mouse and a synthetic pen, checked against what a session
//! would send. Run in a Windows console session (not over SSH or RDP):
//!
//! ```text
//! input_lab.exe OUT.txt
//! ```
//!
//! It writes every [`InputEvent`] and a `PASS`/`FAIL` line per check to
//! `OUT.txt`, and exits non-zero when a check fails. Lab only; never
//! shipped.

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("input_lab runs on Windows only");
    std::process::ExitCode::FAILURE
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    lab::run()
}

#[cfg(windows)]
#[allow(unsafe_code, clippy::too_many_lines)]
mod lab {
    use std::fs::File;
    use std::io::Write;
    use std::process::ExitCode;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use arcen_deck_windows::input::{
        HostInputFacts, InputEvent, InputSink, LAB_KEY_MARK, WindowInput, attach, with_input,
    };
    use arcen_input::PenTool;
    use arcen_input::deck_pen::ImageRect;
    use arcen_input::keyboard::{DeckShortcut, KeyboardGrab, PcKeyboardSettings};
    use arcen_input::text::{HostTextTruth, TypingMode};
    use arcen_protocol::keys::{
        KEY_ALT, KEY_ALTGR, KEY_CONTROL, KEY_META, KEY_SHIFT, MOD_ALT, MOD_CTRL, MOD_KEYPAD,
        MOD_META, MOD_SHIFT,
    };
    use arcen_protocol::messages::{HostPlatform, ScrollUnitMsg};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    use windows::Win32::UI::Input::Ime::{
        CPS_COMPLETE, IMC_SETCONVERSIONMODE, IMC_SETOPENSTATUS, IME_CMODE_FULLSHAPE,
        IME_CMODE_NATIVE, IME_CMODE_ROMAN, ImmGetContext, ImmGetDefaultIMEWnd, ImmNotifyIME,
        ImmReleaseContext, ImmSetCompositionStringW, NI_COMPOSITIONSTR, SCS_SETSTR,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        ACTIVATE_KEYBOARD_LAYOUT_FLAGS, ActivateKeyboardLayout, GetKeyState, GetKeyboardLayoutList,
        HKL, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KLF_ACTIVATE,
        LoadKeyboardLayoutW, MOUSE_EVENT_FLAGS, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MOVE, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, SetFocus, VIRTUAL_KEY,
    };
    use windows::Win32::UI::Input::Pointer::{POINTER_FLAGS, POINTER_PEN_INFO};
    use windows::Win32::UI::TextServices::{
        CLSID_TF_InputProcessorProfiles, CLSID_TF_ThreadMgr,
        GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION, GUID_COMPARTMENT_KEYBOARD_OPENCLOSE,
        GUID_TFCAT_TIP_KEYBOARD, ITfCompartmentMgr, ITfInputProcessorProfileMgr, ITfThreadMgr,
        TF_INPUTPROCESSORPROFILE, TF_IPPMF_DONTCARECURRENTINPUTLANGUAGE, TF_IPPMF_FORPROCESS,
        TF_PROFILETYPE_INPUTPROCESSOR,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, MSG,
        PM_REMOVE, PT_PEN, PeekMessageW, PostMessageW, PostQuitMessage, RegisterClassW,
        SendMessageW, SetCursorPos, SetForegroundWindow, TranslateMessage, WINDOW_EX_STYLE, WM_APP,
        WM_IME_CONTROL, WM_QUIT, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };
    use windows::core::{GUID, Interface, VARIANT, w};

    const MARK: u32 = WM_APP + 1;
    const LAYOUT: u32 = WM_APP + 2;
    const FULLSCREEN: u32 = WM_APP + 3;
    const OTHER_WINDOW: u32 = WM_APP + 4;
    const LOCK: u32 = WM_APP + 5;
    const BACK: u32 = WM_APP + 6;
    const DONE: u32 = WM_APP + 7;
    const IME: u32 = WM_APP + 8;
    const IME_SET: u32 = WM_APP + 9;
    const BAR: u32 = WM_APP + 10;
    // imm.h; not in the crate's IME module.
    const IMC_GETCONVERSIONMODE: u32 = 0x0001;
    const IMC_GETOPENSTATUS: u32 = 0x0005;

    /// Selects the in-box Microsoft IME (Japanese) for this process and says
    /// which keyboard text service is then active.
    fn activate_microsoft_ime() -> windows::core::Result<String> {
        const CLSID_MICROSOFT_IME: GUID =
            GUID::from_u128(0x03B5_835F_F03C_411B_9CE2_AA23_E117_1E36);
        const PROFILE_MICROSOFT_IME: GUID =
            GUID::from_u128(0xA76C_93D9_5523_4E90_AAFA_4DB1_12F9_AC76);
        // SAFETY: COM on this (window) thread; the profile manager is used
        // and released here.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let manager: ITfInputProcessorProfileMgr =
                CoCreateInstance(&CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER)?;
            manager.ActivateProfile(
                TF_PROFILETYPE_INPUTPROCESSOR,
                0x0411,
                &CLSID_MICROSOFT_IME,
                &PROFILE_MICROSOFT_IME,
                HKL::default(),
                TF_IPPMF_FORPROCESS | TF_IPPMF_DONTCARECURRENTINPUTLANGUAGE,
            )?;
            let mut active = TF_INPUTPROCESSORPROFILE::default();
            manager.GetActiveProfile(&GUID_TFCAT_TIP_KEYBOARD, &raw mut active)?;
            Ok(format!(
                "active keyboard profile lang={:#06x} clsid={:?} microsoft_ime={}",
                active.langid,
                active.clsid,
                active.clsid == CLSID_MICROSOFT_IME
            ))
        }
    }

    /// Opens (hiragana, romaji) or closes the active text service through
    /// this thread's TSF keyboard compartments; returns what they then hold.
    fn set_ime_compartments(on: bool) -> windows::core::Result<String> {
        const MODE: i32 = 0x19; // TF_CONVERSIONMODE_NATIVE | FULLSHAPE | ROMAN
        // SAFETY: TSF on this (window) thread; the thread manager stays
        // activated while the input method is on.
        unsafe {
            let (manager, client) = if let Some(active) = TSF.with(|tsf| tsf.borrow().clone()) {
                active
            } else {
                let manager: ITfThreadMgr =
                    CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
                let client = manager.Activate()?;
                TSF.with(|tsf| *tsf.borrow_mut() = Some((manager.clone(), client)));
                (manager, client)
            };
            let compartments: ITfCompartmentMgr = manager.cast()?;
            let open = compartments.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_OPENCLOSE)?;
            let mode =
                compartments.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION)?;
            let open_value = VARIANT::from(i32::from(on));
            open.SetValue(client, &raw const open_value)?;
            if on {
                let mode_value = VARIANT::from(MODE);
                mode.SetValue(client, &raw const mode_value)?;
            }
            let report = format!(
                "tsf_open={} tsf_mode={}",
                open.GetValue()
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                mode.GetValue()
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            );
            if !on {
                TSF.with(|tsf| *tsf.borrow_mut() = None);
                let _ = manager.Deactivate();
            }
            Ok(report)
        }
    }

    fn japanese_in_layout_list() -> bool {
        // SAFETY: the first call sizes the buffer the second fills.
        unsafe {
            let count = GetKeyboardLayoutList(None);
            let mut layouts = vec![HKL::default(); usize::try_from(count).unwrap_or(0)];
            let filled = usize::try_from(GetKeyboardLayoutList(Some(&mut layouts))).unwrap_or(0);
            layouts
                .iter()
                .take(filled)
                .any(|layout| layout.0 as usize & 0xFFFF == 0x0411)
        }
    }

    type Lines = Vec<(String, Option<InputEvent>)>;

    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Lines>>);

    impl Log {
        fn push(&self, line: String, event: Option<InputEvent>) {
            if let Ok(mut lines) = self.0.lock() {
                lines.push((line, event));
            }
        }

        fn segment(&self, mark: u32) -> Vec<InputEvent> {
            let lines = self.0.lock().map(|lines| lines.clone()).unwrap_or_default();
            let wanted = format!("MARK {mark}");
            let Some(start) = lines.iter().position(|(line, _)| *line == wanted) else {
                return Vec::new();
            };
            lines[start + 1..]
                .iter()
                .take_while(|(line, _)| !line.starts_with("MARK "))
                .filter_map(|(_, event)| event.clone())
                .collect()
        }
    }

    struct Sink(Log);

    impl InputSink for Sink {
        fn input(&self, event: InputEvent) {
            self.0.push(format!("{event:?}"), Some(event));
        }
    }

    thread_local! {
        static LOG: std::cell::RefCell<Log> = std::cell::RefCell::new(Log::default());
        static OTHER: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
        static TSF: std::cell::RefCell<Option<(ITfThreadMgr, u32)>> =
            const { std::cell::RefCell::new(None) };
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            MARK => {
                LOG.with(|log| log.borrow().push(format!("MARK {}", wparam.0), None));
                LRESULT(0)
            }
            LAYOUT => {
                let name = if wparam.0 == 1 {
                    w!("00000414")
                } else {
                    w!("00000409")
                };
                // SAFETY: loads and activates a layout for this thread only.
                unsafe {
                    if let Ok(layout) = LoadKeyboardLayoutW(name, KLF_ACTIVATE) {
                        let _ = ActivateKeyboardLayout(layout, ACTIVATE_KEYBOARD_LAYOUT_FLAGS(0));
                    }
                }
                LRESULT(0)
            }
            IME => {
                // Japanese layout with its input method open in hiragana, or
                // back to US with it closed. Logs SKIP when the user has no
                // Japanese input method: the in-box Japanese keyboard layout
                // loads without one, so the user's layout list decides.
                // SAFETY: this thread's layout and input context.
                unsafe {
                    let on = wparam.0 == 1;
                    if on && !japanese_in_layout_list() {
                        LOG.with(|log| {
                            log.borrow().push(
                                "SKIP ime: no Japanese input method in the layout list".to_owned(),
                                None,
                            );
                        });
                        return LRESULT(0);
                    }
                    // The Japanese keyboard layout's HKL selects the plain
                    // keyboard profile; the IME is a TSF text service and is
                    // selected as one, as Win+Space would.
                    let activated = if on {
                        activate_microsoft_ime()
                    } else {
                        LoadKeyboardLayoutW(w!("00000409"), KLF_ACTIVATE).and_then(|layout| {
                            ActivateKeyboardLayout(layout, ACTIVATE_KEYBOARD_LAYOUT_FLAGS(0))
                                .map(|_| "US layout".to_owned())
                        })
                    };
                    match activated {
                        Ok(what) => {
                            LOG.with(|log| log.borrow().push(format!("INFO ime: {what}"), None));
                            let context = ImmGetContext(hwnd);
                            if context.is_invalid() {
                                LOG.with(|log| {
                                    log.borrow()
                                        .push("SKIP ime: no input context".to_owned(), None);
                                });
                            } else {
                                let _ = ImmReleaseContext(hwnd, context);
                                // As IME.ahk and zenhan do (they work with the
                                // Windows 11 Microsoft IME): ask the window's
                                // default IME window. Activating a TSF thread
                                // manager here makes the window TSF-aware with
                                // no document, so keys then bypass the IME
                                // (runs g10-g12); that stays a fallback only.
                                let ime_window = ImmGetDefaultIMEWnd(hwnd);
                                let control = |command: u32, value: isize| {
                                    SendMessageW(
                                        ime_window,
                                        WM_IME_CONTROL,
                                        WPARAM(command as usize),
                                        LPARAM(value),
                                    )
                                    .0
                                };
                                control(IMC_SETOPENSTATUS, isize::from(on));
                                if on {
                                    let mode = IME_CMODE_NATIVE.0
                                        | IME_CMODE_FULLSHAPE.0
                                        | IME_CMODE_ROMAN.0;
                                    control(
                                        IMC_SETCONVERSIONMODE,
                                        isize::try_from(mode).unwrap_or(0),
                                    );
                                }
                                let open = control(IMC_GETOPENSTATUS, 0);
                                let mode = control(IMC_GETCONVERSIONMODE, 0);
                                let tsf = if (on && open == 0)
                                    || (!on && TSF.with(|tsf| tsf.borrow().is_some()))
                                {
                                    set_ime_compartments(on).unwrap_or_else(|error| {
                                        format!("compartments failed: {error}")
                                    })
                                } else {
                                    "tsf=untouched".to_owned()
                                };
                                LOG.with(|log| {
                                    log.borrow().push(
                                        format!(
                                            "INFO ime: ime_window={} open={open} mode={mode:#x} {tsf}",
                                            !ime_window.is_invalid()
                                        ),
                                        None,
                                    );
                                });
                            }
                        }
                        Err(error) => {
                            LOG.with(|log| log.borrow().push(format!("SKIP ime: {error}"), None));
                        }
                    }
                }
                LRESULT(0)
            }
            IME_SET => {
                // The input method's own composition, set and completed
                // through IMM (no key injection): what it then tells the
                // window is what the adapter must turn into preedit and text.
                // SAFETY: this window's input context, released below.
                unsafe {
                    let context = ImmGetContext(hwnd);
                    if !context.is_invalid() {
                        let text: Vec<u16> = "にほん".encode_utf16().collect();
                        let bytes = u32::try_from(text.len() * 2).unwrap_or(0);
                        let set = ImmSetCompositionStringW(
                            context,
                            SCS_SETSTR,
                            Some(text.as_ptr().cast()),
                            bytes,
                            None,
                            0,
                        )
                        .as_bool();
                        let completed =
                            ImmNotifyIME(context, NI_COMPOSITIONSTR, CPS_COMPLETE, 0).as_bool();
                        let _ = ImmReleaseContext(hwnd, context);
                        LOG.with(|log| {
                            log.borrow().push(
                                format!("INFO ime-set: set={set} completed={completed}"),
                                None,
                            );
                        });
                    }
                }
                LRESULT(0)
            }
            FULLSCREEN => {
                with_input(hwnd, |input| input.set_fullscreen(wparam.0 == 1));
                LRESULT(0)
            }
            BAR => {
                // The shell shows (1) or hides (0) the connection bar.
                let shown = wparam.0 == 1;
                let grabbed = with_input(hwnd, |input| {
                    input.set_connection_bar(shown);
                    input.grab_active()
                });
                LOG.with(|log| {
                    log.borrow().push(
                        format!("INFO bar shown={shown} grab_active={grabbed:?}"),
                        None,
                    );
                });
                LRESULT(0)
            }
            LOCK => {
                with_input(hwnd, |input| input.set_pointer_lock(wparam.0 == 1));
                LRESULT(0)
            }
            OTHER_WINDOW => {
                // SAFETY: a second top-level window of this thread takes the
                // focus, as another application would.
                unsafe {
                    let other = CreateWindowExW(
                        WINDOW_EX_STYLE::default(),
                        w!("ArcenInputLab"),
                        w!("other"),
                        WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                        950,
                        100,
                        300,
                        200,
                        None,
                        None,
                        None,
                        None,
                    )
                    .unwrap_or_default();
                    OTHER.with(|cell| cell.set(other.0 as isize));
                    let _ = SetForegroundWindow(other);
                    let _ = SetFocus(other);
                }
                LRESULT(0)
            }
            BACK => {
                let other = OTHER.with(std::cell::Cell::take);
                // SAFETY: the window this thread created for OTHER_WINDOW.
                unsafe {
                    if other != 0 {
                        let _ = DestroyWindow(HWND(other as *mut _));
                    }
                    let _ = SetForegroundWindow(hwnd);
                    let _ = SetFocus(hwnd);
                }
                LRESULT(0)
            }
            DONE => {
                // SAFETY: ends this thread's message loop.
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            // SAFETY: the default procedure for everything else.
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    fn key(scan: u16, extended: bool, up: bool) -> INPUT {
        let mut flags = KEYEVENTF_SCANCODE;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if up {
            flags |= KEYEVENTF_KEYUP;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wScan: scan,
                    dwFlags: flags,
                    dwExtraInfo: LAB_KEY_MARK,
                    ..KEYBDINPUT::default()
                },
            },
        }
    }

    fn vk_tap(vk: u16, extended: bool) {
        let input = |up: bool| {
            let mut flags = KEYBD_EVENT_FLAGS::default();
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if up {
                flags |= KEYEVENTF_KEYUP;
            }
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(vk),
                        dwFlags: flags,
                        dwExtraInfo: LAB_KEY_MARK,
                        ..KEYBDINPUT::default()
                    },
                },
            }
        };
        send(&[input(false), input(true)]);
    }

    fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data.cast_unsigned(),
                    dwFlags: flags,
                    ..MOUSEINPUT::default()
                },
            },
        }
    }

    fn send(inputs: &[INPUT]) {
        // SAFETY: well-formed inputs.
        unsafe {
            SendInput(
                inputs,
                i32::try_from(std::mem::size_of::<INPUT>()).unwrap_or(0),
            )
        };
        std::thread::sleep(Duration::from_millis(80));
    }

    fn tap(scan: u16, extended: bool) {
        send(&[key(scan, extended, false), key(scan, extended, true)]);
    }

    fn post(hwnd: isize, msg: u32, wparam: usize) {
        // SAFETY: posting to the lab window.
        let _ = unsafe { PostMessageW(HWND(hwnd as *mut _), msg, WPARAM(wparam), LPARAM(0)) };
        std::thread::sleep(Duration::from_millis(250));
    }

    // --- Synthetic pen (user32; not in the crate's feature list) -----------

    const POINTER_TYPE_INFO_SIZE: usize = 152;
    const PEN_TAIL: usize = POINTER_TYPE_INFO_SIZE - 8 - std::mem::size_of::<POINTER_PEN_INFO>();

    #[repr(C)]
    struct PointerTypeInfo {
        kind: u32,
        pad: u32,
        pen: POINTER_PEN_INFO,
        tail: [u8; PEN_TAIL],
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn CreateSyntheticPointerDevice(kind: u32, max: u32, mode: u32) -> isize;
        fn InjectSyntheticPointerInput(
            device: isize,
            info: *const PointerTypeInfo,
            count: u32,
        ) -> i32;
        fn DestroySyntheticPointerDevice(device: isize);
    }

    struct SynthPen {
        device: isize,
        frame: u32,
    }

    struct PenFrame {
        flags: u32,
        at: (i32, i32),
        pressure: u32,
        tilt: (i32, i32),
        rotation: u32,
        pen_flags: u32,
    }

    impl SynthPen {
        fn new() -> Option<Self> {
            // SAFETY: PT_PEN, one contact, default feedback.
            let device = unsafe { CreateSyntheticPointerDevice(3, 1, 1) };
            (device != 0).then_some(Self { device, frame: 0 })
        }

        fn send(&mut self, frame: &PenFrame) -> bool {
            self.frame += 1;
            let mut pen = POINTER_PEN_INFO::default();
            pen.pointerInfo.pointerType = PT_PEN;
            pen.pointerInfo.frameId = self.frame;
            pen.pointerInfo.pointerFlags = POINTER_FLAGS(frame.flags);
            pen.pointerInfo.ptPixelLocation = POINT {
                x: frame.at.0,
                y: frame.at.1,
            };
            pen.pointerInfo.ptPixelLocationRaw = pen.pointerInfo.ptPixelLocation;
            pen.penFlags = frame.pen_flags;
            pen.penMask = 0xF;
            pen.pressure = frame.pressure;
            pen.rotation = frame.rotation;
            pen.tiltX = frame.tilt.0;
            pen.tiltY = frame.tilt.1;
            let info = PointerTypeInfo {
                kind: 3,
                pad: 0,
                pen,
                tail: [0; PEN_TAIL],
            };
            // SAFETY: one well-formed pen frame for this device.
            let ok = unsafe { InjectSyntheticPointerInput(self.device, &raw const info, 1) } != 0;
            std::thread::sleep(Duration::from_millis(60));
            ok
        }
    }

    impl Drop for SynthPen {
        fn drop(&mut self) {
            // SAFETY: the device `new` created.
            unsafe { DestroySyntheticPointerDevice(self.device) };
        }
    }

    // --- The run ------------------------------------------------------------

    pub(super) fn run() -> ExitCode {
        let Some(path) = std::env::args().nth(1) else {
            eprintln!("usage: input_lab OUT.txt");
            return ExitCode::FAILURE;
        };
        let Ok(out) = File::create(&path) else {
            return ExitCode::FAILURE;
        };
        let log = LOG.with(|log| log.borrow().clone());
        // SAFETY: an ordinary window class and window on this thread; COM
        // first, as a GUI thread has it, so text services attach to the window.
        let hwnd = unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let instance = GetModuleHandleW(None).unwrap_or_default();
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                lpszClassName: w!("ArcenInputLab"),
                ..WNDCLASSW::default()
            };
            RegisterClassW(&raw const class);
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("ArcenInputLab"),
                w!("Arcen input lab"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                800,
                600,
                None,
                None,
                instance,
                None,
            )
        };
        let Ok(hwnd) = hwnd else {
            return ExitCode::FAILURE;
        };
        let mut input =
            WindowInput::new(hwnd, Box::new(Sink(log.clone())), KeyboardGrab::Fullscreen);
        let facts = HostInputFacts {
            platform: HostPlatform::Windows,
            extended_keys: true,
            precise_scroll: true,
            text_commit: true,
            unicode_text: true,
        };
        input.set_session(Some((facts, PcKeyboardSettings::default())));
        input.set_typing(
            TypingMode::Auto,
            HostTextTruth {
                text_commit: true,
                unicode: true,
            },
        );
        let mut client = RECT::default();
        let mut origin = POINT::default();
        // SAFETY: plain reads of the lab window.
        unsafe {
            let _ = GetClientRect(hwnd, &raw mut client);
            let _ = ClientToScreen(hwnd, &raw mut origin);
        }
        input.set_image_rect(Some(ImageRect {
            left: 0.0,
            top: 0.0,
            width: f64::from(client.right),
            height: f64::from(client.bottom),
        }));
        if attach(hwnd, input).is_err() {
            return ExitCode::FAILURE;
        }
        // SAFETY: the lab window takes the foreground.
        unsafe {
            let _ = SetForegroundWindow(hwnd);
            let _ = SetFocus(hwnd);
        }
        let center = (origin.x + client.right / 2, origin.y + client.bottom / 2);
        let window = hwnd.0 as isize;
        let driver = std::thread::spawn(move || drive(window, center));
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut msg = MSG::default();
        'pump: loop {
            // SAFETY: an ordinary message loop.
            unsafe {
                while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        break 'pump;
                    }
                    let _ = TranslateMessage(&raw const msg);
                    DispatchMessageW(&raw const msg);
                }
            }
            if Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let pen_injected = driver.join().unwrap_or(false);
        let mut report = Report { out, failed: 0 };
        if let Ok(lines) = log.0.lock() {
            for (line, _) in lines.iter() {
                let _ = writeln!(report.out, "{line}");
            }
        }
        checks(&log, &mut report, pen_injected);
        // SAFETY: tidies up the lab window.
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        if report.failed == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }

    fn drive(window: isize, center: (i32, i32)) -> bool {
        std::thread::sleep(Duration::from_millis(800));
        // 1 US layout, windowed (no grab): a plain key, Shift+2, Ctrl+C.
        post(window, LAYOUT, 0);
        post(window, MARK, 1);
        tap(0x1E, false);
        send(&[key(0x2A, false, false), key(0x03, false, false)]);
        send(&[key(0x03, false, true), key(0x2A, false, true)]);
        send(&[key(0x1D, false, false), key(0x2E, false, false)]);
        send(&[key(0x2E, false, true), key(0x1D, false, true)]);
        // 2 Norwegian layout: AltGr+2 types "@"; no Control reaches the host.
        post(window, LAYOUT, 1);
        post(window, MARK, 2);
        send(&[key(0x38, true, false), key(0x03, false, false)]);
        send(&[key(0x03, false, true), key(0x38, true, true)]);
        post(window, LAYOUT, 0);
        // 3 Without the grab: the Windows key stays local; Ctrl+Win+K is the
        // Deck's, and Start stays shut.
        post(window, MARK, 3);
        send(&[
            key(0x1D, false, false),
            key(0x5B, true, false),
            key(0x25, false, false),
        ]);
        send(&[
            key(0x25, false, true),
            key(0x5B, true, true),
            key(0x1D, false, true),
        ]);
        // 4 With the grab (fullscreen): the Windows key and Alt+Tab go to the
        // host.
        post(window, FULLSCREEN, 1);
        post(window, MARK, 4);
        tap(0x5B, true);
        send(&[key(0x38, false, false), key(0x0F, false, false)]);
        send(&[key(0x0F, false, true), key(0x38, false, true)]);
        // ... and AltGr still types under the grab (Norwegian "@").
        post(window, LAYOUT, 1);
        send(&[key(0x38, true, false), key(0x03, false, false)]);
        send(&[key(0x03, false, true), key(0x38, true, true)]);
        post(window, LAYOUT, 0);
        // 15 Ctrl+Alt+Home under the grab is the Deck's; the shell shows the
        // connection bar, which takes the keyboard, so a letter stays with
        // the Deck; 16 once the bar closes,
        // the grab is back and the Windows key reaches the host again.
        post(window, MARK, 15);
        send(&[
            key(0x1D, false, false),
            key(0x38, false, false),
            key(0x47, true, false),
        ]);
        send(&[
            key(0x47, true, true),
            key(0x38, false, true),
            key(0x1D, false, true),
        ]);
        post(window, BAR, 1);
        tap(0x1E, false);
        post(window, BAR, 0);
        post(window, MARK, 16);
        tap(0x5B, true);
        post(window, FULLSCREEN, 0);
        // 5 F9 resets; Ctrl+Alt+End is Send Ctrl+Alt+Del.
        post(window, MARK, 5);
        tap(0x43, false);
        send(&[
            key(0x1D, false, false),
            key(0x38, false, false),
            key(0x4F, true, false),
        ]);
        send(&[
            key(0x4F, true, true),
            key(0x38, false, true),
            key(0x1D, false, true),
        ]);
        // 6 Keypad 7: a keypad digit with Num Lock on, Home with it off.
        // SAFETY: a plain read of the toggle state.
        let num_on = unsafe { GetKeyState(0x90) } & 1 != 0;
        if !num_on {
            vk_tap(0x90, true);
        }
        post(window, MARK, 6);
        tap(0x47, false);
        vk_tap(0x90, true);
        tap(0x47, false);
        vk_tap(0x90, true);
        if !num_on {
            vk_tap(0x90, true);
        }
        // 7 Focus loss with Shift held releases it on the host.
        post(window, MARK, 7);
        send(&[key(0x2A, false, false)]);
        post(window, OTHER_WINDOW, 0);
        send(&[key(0x2A, false, true)]);
        post(window, BACK, 0);
        // 8 Mouse: a move, a click, a notch and a fine wheel step.
        // SAFETY: places the system cursor at the lab window's centre.
        unsafe {
            let _ = SetCursorPos(center.0, center.1);
        }
        post(window, MARK, 8);
        send(&[mouse(1, 0, 0, MOUSEEVENTF_MOVE)]);
        send(&[mouse(0, 0, 0, MOUSEEVENTF_LEFTDOWN)]);
        send(&[mouse(0, 0, 0, MOUSEEVENTF_LEFTUP)]);
        send(&[mouse(0, 0, 120, MOUSEEVENTF_WHEEL)]);
        send(&[mouse(0, 0, 30, MOUSEEVENTF_WHEEL)]);
        // 9 Pointer lock: raw relative motion; F9 releases the lock.
        post(window, LOCK, 1);
        post(window, MARK, 9);
        send(&[mouse(10, 5, 0, MOUSEEVENTF_MOVE)]);
        tap(0x43, false);
        // The cursor's last move is delivered before the pen starts.
        send(&[mouse(1, 0, 0, MOUSEEVENTF_MOVE)]);
        std::thread::sleep(Duration::from_millis(300));
        // 10 Pen: hover, contact with pressure, tilt and rotation, the barrel
        // button, the eraser end, out of range.
        post(window, MARK, 10);
        let injected = SynthPen::new().is_some_and(|mut pen| {
            const INRANGE: u32 = 0x2;
            const INCONTACT: u32 = 0x4;
            const DOWN: u32 = 0x1_0000;
            const UPDATE: u32 = 0x2_0000;
            const UP: u32 = 0x4_0000;
            let (x, y) = center;
            let frame = |flags, at, pressure, pen_flags| PenFrame {
                flags,
                at,
                pressure,
                tilt: if pressure > 0 { (30, -20) } else { (0, 0) },
                rotation: if pressure > 0 { 90 } else { 0 },
                pen_flags,
            };
            let moved = (x + 20, y + 10);
            let frames = [
                frame(INRANGE | UPDATE, (x, y), 0, 0),
                frame(INRANGE | INCONTACT | DOWN, (x, y), 512, 0),
                frame(INRANGE | INCONTACT | UPDATE, moved, 1024, 1),
                frame(INRANGE | UP, moved, 0, 0),
                frame(UPDATE, moved, 0, 0),
                // The eraser end comes into range as a new approach.
                frame(INRANGE | UPDATE, moved, 0, 2),
                frame(INRANGE | INCONTACT | DOWN, moved, 300, 6),
                frame(INRANGE | INCONTACT | UPDATE, moved, 300, 6),
                frame(INRANGE | UP, moved, 0, 2),
                frame(UPDATE, moved, 0, 2),
            ];
            let mut ok = true;
            for frame in &frames {
                ok &= pen.send(frame);
            }
            ok
        });
        // 11 Input method: Japanese romaji "nihon", Space converts, Enter
        // commits; the keys stay with the input method.
        post(window, IME, 1);
        std::thread::sleep(Duration::from_millis(500));
        post(window, MARK, 11);
        for scan in [0x31, 0x17, 0x23, 0x18, 0x31] {
            tap(scan, false);
        }
        std::thread::sleep(Duration::from_millis(300));
        tap(0x39, false);
        std::thread::sleep(Duration::from_millis(500));
        tap(0x1C, false);
        std::thread::sleep(Duration::from_millis(500));
        // The same romaji as virtual keys (as on-screen keyboards inject).
        post(window, MARK, 14);
        for vk in [b'N', b'I', b'H', b'O', b'N'] {
            vk_tap(u16::from(vk), false);
        }
        std::thread::sleep(Duration::from_millis(300));
        vk_tap(0x20, false);
        std::thread::sleep(Duration::from_millis(500));
        vk_tap(0x0D, false);
        std::thread::sleep(Duration::from_millis(500));
        post(window, MARK, 12);
        post(window, IME_SET, 0);
        std::thread::sleep(Duration::from_millis(500));
        post(window, MARK, 13);
        post(window, IME, 0);
        post(window, DONE, 0);
        injected
    }

    // --- Checks ---------------------------------------------------------------

    struct Report {
        out: File,
        failed: usize,
    }

    impl Report {
        fn check(&mut self, name: &str, ok: bool, detail: &str) {
            let line = format!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
            println!("{line}");
            let _ = writeln!(self.out, "{line}");
            if !ok {
                self.failed += 1;
            }
        }
    }

    fn keys(events: &[InputEvent]) -> Vec<(u32, bool, u32)> {
        events
            .iter()
            .filter_map(|event| match event {
                InputEvent::Keys(edges) => Some(edges.clone()),
                _ => None,
            })
            .flatten()
            .map(|edge| (edge.qt_key, edge.pressed, edge.modifiers))
            .collect()
    }

    fn checks(log: &Log, report: &mut Report, pen_injected: bool) {
        let s1 = keys(&log.segment(1));
        report.check(
            "us-plain-shift-ctrl",
            s1 == [
                (0x41, true, 0),
                (0x41, false, 0),
                (KEY_SHIFT, true, MOD_SHIFT),
                (0x32, true, MOD_SHIFT),
                (0x32, false, MOD_SHIFT),
                (KEY_SHIFT, false, 0),
                (KEY_CONTROL, true, MOD_CTRL),
                (0x43, true, MOD_CTRL),
                (0x43, false, MOD_CTRL),
                (KEY_CONTROL, false, 0),
            ],
            &format!("{s1:x?}"),
        );
        let s2 = log.segment(2);
        let s2_keys = keys(&s2);
        report.check(
            "nb-altgr-2-types-at",
            s2.contains(&InputEvent::Text("@".to_owned()))
                && !s2_keys
                    .iter()
                    .any(|(key, ..)| *key == KEY_CONTROL || *key == KEY_ALTGR),
            &format!("{s2:?}"),
        );
        let s3 = log.segment(3);
        report.check(
            "no-grab-windows-local-ctrl-win-k-deck",
            s3.contains(&InputEvent::Shortcut(DeckShortcut::KeyboardPhysical))
                && !keys(&s3).iter().any(|(key, ..)| *key == KEY_META),
            &format!("{s3:?}"),
        );
        let s4_events = log.segment(4);
        let s4 = keys(&s4_events);
        report.check(
            "grab-windows-alt-tab-to-host-altgr-still-types",
            s4.contains(&(KEY_META, true, MOD_META))
                && s4.contains(&(KEY_ALT, true, MOD_ALT))
                && s4.contains(&(0x0100_0001, true, MOD_ALT))
                && s4_events.contains(&InputEvent::Text("@".to_owned()))
                && !s4
                    .iter()
                    .any(|(key, ..)| *key == KEY_CONTROL || *key == KEY_ALTGR),
            &format!("{s4_events:?}"),
        );
        let s15_events = log.segment(15);
        let s15 = keys(&s15_events);
        let s16 = keys(&log.segment(16));
        report.check(
            "ctrl-alt-home-connection-bar-takes-the-keyboard",
            s15_events.contains(&InputEvent::Shortcut(DeckShortcut::ShowConnectionBar))
                && !s15.iter().any(|(key, pressed, _)| *pressed && *key == 0x41)
                && !s15.iter().any(|(key, ..)| *key == 0x0100_0010)
                && s16.contains(&(KEY_META, true, MOD_META)),
            &format!("{s15_events:?} then {s16:?}"),
        );
        let s5 = log.segment(5);
        report.check(
            "f9-reset-and-ctrl-alt-end",
            s5.iter()
                .any(|event| matches!(event, InputEvent::Reset { reason: "panic_f9" }))
                && s5.contains(&InputEvent::Shortcut(DeckShortcut::SendSecureAttention))
                && !keys(&s5).iter().any(|(key, ..)| *key == 0x0100_0038),
            &format!("{s5:?}"),
        );
        let s6 = keys(&log.segment(6));
        report.check(
            "keypad-follows-num-lock",
            s6.contains(&(0x37, true, MOD_KEYPAD))
                && s6.contains(&(0x0100_0010, true, 0))
                && !s6.iter().any(|(key, ..)| *key == 0x0100_0025),
            &format!("{s6:x?}"),
        );
        let s7 = keys(&log.segment(7));
        report.check(
            "focus-loss-releases-shift",
            s7 == [(KEY_SHIFT, true, MOD_SHIFT), (KEY_SHIFT, false, 0)],
            &format!("{s7:x?}"),
        );
        let s8 = log.segment(8);
        let moved = s8
            .iter()
            .any(|event| matches!(event, InputEvent::PointerMove { .. }));
        let clicks = s8
            .iter()
            .filter(|event| matches!(event, InputEvent::Button { button: 1, .. }))
            .count();
        let scrolls: Vec<_> = s8
            .iter()
            .filter_map(|event| match event {
                InputEvent::Scroll { scroll, .. } => Some((scroll.unit, scroll.dy)),
                _ => None,
            })
            .collect();
        report.check(
            "mouse-move-click-wheel",
            moved
                && clicks == 2
                && scrolls == [(ScrollUnitMsg::Line, 1.0), (ScrollUnitMsg::Point, 30.0)],
            &format!("{s8:?}"),
        );
        let s9 = log.segment(9);
        report.check(
            "pointer-lock-relative-and-f9",
            s9.contains(&InputEvent::PointerRelative { dx: 10, dy: 5 })
                && s9
                    .iter()
                    .any(|event| matches!(event, InputEvent::PointerLockReleased { .. })),
            &format!("{s9:?}"),
        );
        let s10 = log.segment(10);
        let pens: Vec<_> = s10
            .iter()
            .filter_map(|event| match event {
                InputEvent::Pen(pen) => Some(*pen),
                _ => None,
            })
            .collect();
        let hover = pens
            .first()
            .is_some_and(|pen| pen.in_proximity && !pen.touching);
        let leaves = pens.iter().filter(|pen| !pen.in_proximity).count();
        let contact = pens.iter().any(|pen| {
            pen.touching
                && (pen.pressure - 0.5).abs() < 0.01
                && (pen.tilt_x_degrees - 30.0).abs() < 0.5
                && (pen.tilt_y_degrees + 20.0).abs() < 0.5
                && (pen.rotation_degrees - 90.0).abs() < 0.5
        });
        let barrel = pens.iter().any(|pen| pen.buttons == 1);
        let eraser = pens.iter().any(|pen| pen.tool == PenTool::Eraser);
        let leave = pens.last().is_some_and(|pen| !pen.in_proximity);
        let no_mouse = !s10.iter().any(|event| {
            matches!(
                event,
                InputEvent::PointerMove { .. } | InputEvent::Button { .. }
            )
        });
        let skipped = log
            .0
            .lock()
            .is_ok_and(|lines| lines.iter().any(|(line, _)| line.starts_with("SKIP ime")));
        // Romaji "nihon" converts to 日本 (or stays にほん); kana input would
        // type other characters. Scancodes (11) and virtual keys (14).
        let composed = |segment: &[InputEvent]| {
            let preedit = segment
                .iter()
                .any(|event| matches!(event, InputEvent::Preedit(Some(text)) if !text.is_empty()));
            let committed: String = segment
                .iter()
                .filter_map(|event| match event {
                    InputEvent::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let cjk = committed.contains('日') || committed.contains("にほん");
            let letters_sent = keys(segment)
                .iter()
                .any(|(key, pressed, _)| *pressed && (0x41..=0x5A).contains(key));
            (
                preedit && cjk && !letters_sent,
                format!(
                    "preedit={preedit} committed={committed:?} letters_sent={letters_sent} {segment:?}"
                ),
            )
        };
        for (segment, how) in [(11, "scancodes"), (14, "virtual-keys")] {
            let (ok, detail) = composed(&log.segment(segment));
            report.check(
                &if skipped {
                    format!("ime-composition-{how} (SKIPPED: no Japanese input method)")
                } else {
                    format!("ime-composition-{how}-preedit-and-commit")
                },
                skipped || ok,
                &detail,
            );
        }
        let s12 = log.segment(12);
        let set_preedit = s12.iter().any(
            |event| matches!(event, InputEvent::Preedit(Some(text)) if text.contains("にほん")),
        );
        let set_text: String = s12
            .iter()
            .filter_map(|event| match event {
                InputEvent::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        report.check(
            if skipped {
                "ime-composition-through-imm (SKIPPED: no Japanese input method)"
            } else {
                "ime-composition-through-imm-preedit-and-commit"
            },
            skipped || (set_preedit && set_text.contains("にほん")),
            &format!("preedit={set_preedit} committed={set_text:?} {s12:?}"),
        );
        report.check(
            "pen-pressure-tilt-rotation-barrel-eraser-leave",
            pen_injected
                && hover
                && contact
                && barrel
                && eraser
                && leave
                && leaves == 2
                && no_mouse,
            &format!(
                "injected={pen_injected} hover={hover} contact={contact} barrel={barrel} \
                 eraser={eraser} leave={leave} no_mouse={no_mouse} {pens:?}"
            ),
        );
    }
}
