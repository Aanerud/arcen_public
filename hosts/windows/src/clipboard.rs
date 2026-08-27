//! User-session Win32 clipboard adapter and bounded transport queues.

use arcen_media::clipboard::{
    dibv5_to_png, png_to_dibv5, ClipboardContent, ClipboardDirection, ClipboardFlow, ClipboardKind,
    ClipboardPolicy, EchoMarker, EchoSuppressor, EchoToken, ImageLimits, MAX_DECODED_IMAGE_BYTES,
};
use arcen_protocol::messages::{
    ClientHelloMsg, ClipboardContentKind, ClipboardContentMsg, ClipboardDataMsg,
    ClipboardDirectionMsg, ClipboardPolicyMsg, CLIPBOARD_PROTOCOL_VERSION,
};
use arcen_protocol::{encode_clipboard_chunk, ClipboardChunkHeader, CHUNK_BYTES};
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;
use zeroize::Zeroize;

const CF_UNICODETEXT_VALUE: u32 = 13;
const CF_DIBV5_VALUE: u32 = 17;
const WM_ARCEN_CLIPBOARD_WAKE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 0x431;
const WM_ARCEN_CLIPBOARD_STOP: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 0x432;
const OPEN_RETRY_DELAYS_MS: [u64; 8] = [5, 10, 20, 40, 80, 80, 80, 80];

/// Host policy intersected with exact Deck v1 capability bits.
#[derive(Debug, Clone, Copy)]
pub struct ClipboardNegotiation {
    policy: ClipboardPolicy,
    text_c2h: bool,
    text_h2c: bool,
    image_c2h: bool,
    image_h2c: bool,
}

impl ClipboardNegotiation {
    #[must_use]
    pub fn from_client(policy: ClipboardPolicy, hello: &ClientHelloMsg) -> Option<Self> {
        if hello.clipboard_protocol_version != CLIPBOARD_PROTOCOL_VERSION
            || matches!(policy.direction, ClipboardDirection::Disabled)
        {
            return None;
        }
        let negotiated = Self {
            policy,
            text_c2h: hello.clipboard_text_c2s,
            text_h2c: hello.clipboard_text_s2c,
            image_c2h: hello.clipboard_image_c2s,
            image_h2c: hello.clipboard_image_s2c,
        };
        (negotiated.allows(ClipboardFlow::ClientToHost, ClipboardContentKind::TextUtf8)
            || negotiated.allows(ClipboardFlow::ClientToHost, ClipboardContentKind::ImagePng)
            || negotiated.allows(ClipboardFlow::HostToClient, ClipboardContentKind::TextUtf8)
            || negotiated.allows(ClipboardFlow::HostToClient, ClipboardContentKind::ImagePng))
        .then_some(negotiated)
    }

    #[must_use]
    pub const fn policy(self) -> ClipboardPolicy {
        self.policy
    }

    #[must_use]
    pub fn allows(self, flow: ClipboardFlow, kind: ClipboardContentKind) -> bool {
        if !self.policy.allows(flow, media_kind(kind)) {
            return false;
        }
        match (flow, kind) {
            (ClipboardFlow::ClientToHost, ClipboardContentKind::TextUtf8) => self.text_c2h,
            (ClipboardFlow::HostToClient, ClipboardContentKind::TextUtf8) => self.text_h2c,
            (ClipboardFlow::ClientToHost, ClipboardContentKind::ImagePng) => self.image_c2h,
            (ClipboardFlow::HostToClient, ClipboardContentKind::ImagePng) => self.image_h2c,
        }
    }
}

#[must_use]
pub fn policy_message(policy: ClipboardPolicy) -> ClipboardPolicyMsg {
    ClipboardPolicyMsg {
        protocol_version: CLIPBOARD_PROTOCOL_VERSION,
        direction: match policy.direction {
            ClipboardDirection::Both => ClipboardDirectionMsg::Both,
            ClipboardDirection::ClientToHost => ClipboardDirectionMsg::ClientToHost,
            ClipboardDirection::HostToClient => ClipboardDirectionMsg::HostToClient,
            ClipboardDirection::Disabled => ClipboardDirectionMsg::Disabled,
        },
        content: match policy.content {
            ClipboardContent::All => ClipboardContentMsg::All,
            ClipboardContent::Text => ClipboardContentMsg::Text,
            ClipboardContent::Image => ClipboardContentMsg::Image,
        },
        max_bytes: u32::try_from(policy.max_bytes)
            .expect("validated clipboard policy always fits u32"),
    }
}

fn media_kind(kind: ClipboardContentKind) -> ClipboardKind {
    match kind {
        ClipboardContentKind::TextUtf8 => ClipboardKind::TextUtf8,
        ClipboardContentKind::ImagePng => ClipboardKind::ImagePng,
    }
}

/// One owned clipboard payload. Debug output exposes metadata only.
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

struct OutboundTransfer {
    item: ClipboardItem,
    offer_sent: bool,
    offset: usize,
}

impl OutboundTransfer {
    fn next_message(&mut self) -> Result<Message, String> {
        if !self.offer_sent {
            self.offer_sent = true;
            let size = u32::try_from(self.item.bytes.len())
                .map_err(|_| "clipboard payload size exceeds u32".to_string())?;
            let offer = ClipboardDataMsg::new(
                self.item.sequence,
                self.item.kind,
                size,
                self.item.truncated,
            );
            return serde_json::to_string(&offer)
                .map(|text| Message::Text(text.into()))
                .map_err(|error| format!("serialize clipboard offer: {error}"));
        }
        let end = self
            .offset
            .checked_add(CHUNK_BYTES)
            .unwrap_or(self.item.bytes.len())
            .min(self.item.bytes.len());
        let frame = encode_clipboard_chunk(
            ClipboardChunkHeader {
                kind: self.item.kind,
                sequence: self.item.sequence,
                total_size: u32::try_from(self.item.bytes.len())
                    .map_err(|_| "clipboard payload size exceeds u32".to_string())?,
                offset: u32::try_from(self.offset)
                    .map_err(|_| "clipboard offset exceeds u32".to_string())?,
            },
            &self.item.bytes[self.offset..end],
        )
        .map_err(|error| format!("encode clipboard chunk: {error:?}"))?;
        self.offset = end;
        Ok(Message::Binary(frame.into()))
    }

    const fn finished(&self) -> bool {
        self.offer_sent && self.offset == self.item.bytes.len()
    }
}

#[derive(Default)]
struct WriterState {
    latest_sequence: u64,
    pending: Option<ClipboardItem>,
    active: Option<OutboundTransfer>,
    closed: bool,
}

impl Drop for WriterState {
    fn drop(&mut self) {
        if let Some(mut item) = self.pending.take() {
            item.scrub();
        }
        self.active = None;
    }
}

/// One active transfer plus one replaceable pending item.
pub struct ClipboardWriterQueue {
    state: Mutex<WriterState>,
    ready: Notify,
}

impl ClipboardWriterQueue {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(WriterState::default()),
            ready: Notify::new(),
        })
    }

    pub fn enqueue(&self, mut item: ClipboardItem) -> bool {
        let mut state = self.lock_state();
        if state.closed || item.sequence <= state.latest_sequence {
            item.scrub();
            return false;
        }
        state.latest_sequence = item.sequence;
        if let Some(mut pending) = state.pending.take() {
            pending.scrub();
        }
        state.pending = Some(item);
        drop(state);
        self.ready.notify_one();
        true
    }

    pub async fn pop(&self) -> Result<Option<Message>, String> {
        loop {
            {
                let mut state = self.lock_state();
                if state
                    .pending
                    .as_ref()
                    .zip(state.active.as_ref())
                    .is_some_and(|(pending, active)| pending.sequence > active.item.sequence)
                {
                    state.active = None;
                }
                if state.active.is_none() {
                    state.active = state.pending.take().map(|item| OutboundTransfer {
                        item,
                        offer_sent: false,
                        offset: 0,
                    });
                }
                if let Some(active) = state.active.as_mut() {
                    let message = active.next_message()?;
                    if active.finished() {
                        state.active = None;
                    }
                    return Ok(Some(message));
                }
                if state.closed {
                    return Ok(None);
                }
            }
            self.ready.notified().await;
        }
    }

    pub fn close(&self) {
        let mut state = self.lock_state();
        state.closed = true;
        if let Some(mut pending) = state.pending.take() {
            pending.scrub();
        }
        state.active = None;
        drop(state);
        self.ready.notify_one();
    }

    #[cfg(test)]
    fn bounded_counts(&self) -> (usize, usize) {
        let state = self.lock_state();
        (
            usize::from(state.active.is_some()),
            usize::from(state.pending.is_some()),
        )
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, WriterState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct ThreadState {
    negotiation: ClipboardNegotiation,
    policy: ClipboardPolicy,
    outbound: Arc<ClipboardWriterQueue>,
    inbound: Mutex<Option<ClipboardItem>>,
    hwnd: AtomicPtr<std::ffi::c_void>,
    marker_format: AtomicU32,
    next_sequence: AtomicU64,
    last_native_sequence: AtomicU32,
    thread_id: AtomicU32,
    wake_pending: AtomicBool,
    stopping: AtomicBool,
    suppressor: Mutex<EchoSuppressor>,
}

impl ThreadState {
    fn next_sequence(&self) -> Option<u64> {
        self.next_sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .ok()
            .filter(|sequence| *sequence != 0)
    }

    fn replace_inbound(&self, mut item: ClipboardItem) -> bool {
        let mut inbound = self
            .inbound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inbound
            .as_ref()
            .is_some_and(|pending| pending.sequence >= item.sequence)
        {
            item.scrub();
            return false;
        }
        if let Some(mut pending) = inbound.take() {
            pending.scrub();
        }
        *inbound = Some(item);
        true
    }
}

static WINDOWS: OnceLock<Mutex<HashMap<usize, Weak<ThreadState>>>> = OnceLock::new();

fn windows() -> &'static Mutex<HashMap<usize, Weak<ThreadState>>> {
    WINDOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Dedicated user-session clipboard listener thread.
pub struct WindowsClipboardRuntime {
    state: Arc<ThreadState>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WindowsClipboardRuntime {
    pub fn start(
        negotiation: ClipboardNegotiation,
        outbound: Arc<ClipboardWriterQueue>,
    ) -> Result<Self, String> {
        let mut token = [0u8; 16];
        getrandom::getrandom(&mut token)
            .map_err(|error| format!("clipboard origin randomness: {error}"))?;
        let state = Arc::new(ThreadState {
            negotiation,
            policy: negotiation.policy(),
            outbound,
            inbound: Mutex::new(None),
            hwnd: AtomicPtr::new(std::ptr::null_mut()),
            marker_format: AtomicU32::new(0),
            next_sequence: AtomicU64::new(1),
            last_native_sequence: AtomicU32::new(0),
            thread_id: AtomicU32::new(0),
            wake_pending: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            suppressor: Mutex::new(EchoSuppressor::new(EchoToken(token))),
        });
        let thread_state = Arc::clone(&state);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("arcen-clipboard".to_string())
            .spawn(move || clipboard_thread(thread_state, ready_tx))
            .map_err(|error| format!("start clipboard thread: {error}"))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                state,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err("clipboard thread ended before readiness".to_string())
            }
        }
    }

    pub fn inject(&self, item: ClipboardItem) -> bool {
        if self.state.stopping.load(Ordering::Acquire) {
            return false;
        }
        if !self.state.replace_inbound(item) {
            return false;
        }
        if self.state.wake_pending.swap(true, Ordering::AcqRel) {
            return true;
        }
        let hwnd = self.state.hwnd.load(Ordering::Acquire);
        if hwnd.is_null() {
            self.state.wake_pending.store(false, Ordering::Release);
            return false;
        }
        // SAFETY: the message contains no pointer; the HWND remains owned until shutdown joins.
        let posted = unsafe {
            windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                windows::Win32::Foundation::HWND(hwnd),
                WM_ARCEN_CLIPBOARD_WAKE,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            )
        }
        .is_ok();
        if !posted {
            self.state.wake_pending.store(false, Ordering::Release);
        }
        posted
    }

    pub fn shutdown(&mut self) {
        self.state.stopping.store(true, Ordering::Release);
        let hwnd = self.state.hwnd.load(Ordering::Acquire);
        let mut posted = false;
        if !hwnd.is_null() {
            // SAFETY: the message contains no pointer; the HWND is joined below.
            posted = unsafe {
                windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    windows::Win32::Foundation::HWND(hwnd),
                    WM_ARCEN_CLIPBOARD_STOP,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                )
            }
            .is_ok();
        }
        if !posted {
            let thread_id = self.state.thread_id.load(Ordering::Acquire);
            if thread_id != 0 {
                // SAFETY: thread id belongs to the joined clipboard thread and
                // the message carries no pointers.
                let _ = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                        thread_id,
                        WM_ARCEN_CLIPBOARD_STOP,
                        windows::Win32::Foundation::WPARAM(0),
                        windows::Win32::Foundation::LPARAM(0),
                    )
                };
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.state.outbound.close();
        if let Ok(mut inbound) = self.state.inbound.lock() {
            if let Some(mut item) = inbound.take() {
                item.scrub();
            }
        }
    }
}

impl Drop for WindowsClipboardRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn clipboard_thread(
    state: Arc<ThreadState>,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    // SAFETY: returns the numeric identity of the current clipboard thread.
    state.thread_id.store(
        unsafe { windows::Win32::System::Threading::GetCurrentThreadId() },
        Ordering::Release,
    );
    let result = create_clipboard_window(&state);
    let hwnd = match result {
        Ok(hwnd) => hwnd,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    state.hwnd.store(hwnd.0, Ordering::Release);
    windows()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(hwnd.0 as usize, Arc::downgrade(&state));

    // SAFETY: hwnd is a live message-only window on this thread.
    if let Err(error) =
        unsafe { windows::Win32::System::DataExchange::AddClipboardFormatListener(hwnd) }
    {
        let _ = ready.send(Err(format!("register clipboard listener: {error}")));
        cleanup_window(hwnd, &state);
        return;
    }
    let _ = ready.send(Ok(()));

    // SAFETY: MSG storage is initialized; GetMessage/Translate/Dispatch are used
    // on this window's owning thread until a stop message destroys the window.
    unsafe {
        let mut message = windows::Win32::UI::WindowsAndMessaging::MSG::default();
        loop {
            let status =
                windows::Win32::UI::WindowsAndMessaging::GetMessageW(&raw mut message, None, 0, 0)
                    .0;
            if status <= 0 {
                break;
            }
            if state.stopping.load(Ordering::Acquire) || message.message == WM_ARCEN_CLIPBOARD_STOP
            {
                break;
            }
            let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&raw const message);
            windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&raw const message);
        }
    }
    cleanup_window(hwnd, &state);
}

fn cleanup_window(hwnd: windows::Win32::Foundation::HWND, state: &ThreadState) {
    state.hwnd.store(std::ptr::null_mut(), Ordering::Release);
    windows()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(hwnd.0 as usize));
    // SAFETY: hwnd was created on this thread and is no longer dispatched after cleanup.
    unsafe {
        let _ = windows::Win32::System::DataExchange::RemoveClipboardFormatListener(hwnd);
        let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
    }
}

fn create_clipboard_window(
    state: &ThreadState,
) -> Result<windows::Win32::Foundation::HWND, String> {
    use windows::core::w;
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, RegisterClassW, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW,
    };

    // SAFETY: static UTF-16 format name is valid and NUL-terminated.
    let marker_format = unsafe { RegisterClipboardFormatW(w!("ArcenClipboardOrigin")) };
    if marker_format == 0 {
        return Err("register ArcenClipboardOrigin format failed".to_string());
    }
    state.marker_format.store(marker_format, Ordering::Release);
    // SAFETY: None requests the current process module.
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| format!("clipboard module lookup: {error}"))?;
    let instance = HINSTANCE(module.0);
    let class = WNDCLASSW {
        lpfnWndProc: Some(clipboard_window_proc),
        hInstance: instance,
        lpszClassName: w!("ArcenClipboardListenerWindow"),
        ..Default::default()
    };
    // SAFETY: class points to static names and a valid callback.
    let _ = unsafe { RegisterClassW(&raw const class) };
    // SAFETY: the class and instance are valid; no raw application pointer is passed.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("ArcenClipboardListenerWindow"),
            w!("Arcen Clipboard"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            instance,
            None,
        )
    }
    .map_err(|error| format!("create clipboard message window: {error}"))
}

unsafe extern "system" fn clipboard_window_proc(
    hwnd: windows::Win32::Foundation::HWND,
    message: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        clipboard_window_proc_inner(hwnd, message, wparam, lparam)
    }));
    match result {
        Ok(result) => result,
        Err(_) => {
            // SAFETY: no borrowed data crosses the callback; quit tears down this thread.
            unsafe { windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(1) };
            windows::Win32::Foundation::LRESULT(0)
        }
    }
}

fn clipboard_window_proc_inner(
    hwnd: windows::Win32::Foundation::HWND,
    message: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, PostQuitMessage, WM_CLIPBOARDUPDATE,
    };

    let state = windows()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(hwnd.0 as usize))
        .and_then(Weak::upgrade);
    if let Some(state) = state {
        match message {
            WM_CLIPBOARDUPDATE => {
                observe_local_clipboard(hwnd, &state);
                return windows::Win32::Foundation::LRESULT(0);
            }
            WM_ARCEN_CLIPBOARD_WAKE => {
                state.wake_pending.store(false, Ordering::Release);
                inject_remote_clipboard(hwnd, &state);
                return windows::Win32::Foundation::LRESULT(0);
            }
            WM_ARCEN_CLIPBOARD_STOP => {
                // SAFETY: this posts thread-local WM_QUIT without pointer arguments.
                unsafe { PostQuitMessage(0) };
                return windows::Win32::Foundation::LRESULT(0);
            }
            _ => {}
        }
    }
    // SAFETY: unhandled messages are forwarded unchanged to the system default.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn observe_local_clipboard(hwnd: windows::Win32::Foundation::HWND, state: &ThreadState) {
    use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;

    // SAFETY: the function has no pointers and returns the desktop clipboard sequence.
    let native_sequence = unsafe { GetClipboardSequenceNumber() };
    if native_sequence == 0
        || state
            .last_native_sequence
            .swap(native_sequence, Ordering::AcqRel)
            == native_sequence
    {
        return;
    }
    if !open_clipboard_with_retries(
        || {
            // SAFETY: hwnd is the live listener window on the calling thread.
            unsafe { windows::Win32::System::DataExchange::OpenClipboard(hwnd) }.is_ok()
        },
        std::thread::sleep,
    ) {
        return;
    }
    let _close = ClipboardCloseGuard;
    let marker_format = state.marker_format.load(Ordering::Acquire);
    if let Some(marker) = read_marker(marker_format) {
        let suppressor = state
            .suppressor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if suppressor.should_suppress(marker) {
            return;
        }
    }
    let Some(sequence) = state.next_sequence() else {
        return;
    };
    let item = read_local_item(sequence, state.negotiation);
    if let Some(item) = item {
        let _ = state.outbound.enqueue(item);
    }
}

fn inject_remote_clipboard(hwnd: windows::Win32::Foundation::HWND, state: &ThreadState) {
    let item = state
        .inbound
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let Some(mut item) = item else {
        return;
    };
    if !state
        .negotiation
        .allows(ClipboardFlow::ClientToHost, item.kind)
        || state
            .policy
            .check_size(
                ClipboardFlow::ClientToHost,
                media_kind(item.kind),
                item.bytes.len(),
            )
            .is_err()
    {
        item.scrub();
        return;
    }
    if !open_clipboard_with_retries(
        || {
            // SAFETY: hwnd is the live listener window on the calling thread.
            unsafe { windows::Win32::System::DataExchange::OpenClipboard(hwnd) }.is_ok()
        },
        std::thread::sleep,
    ) {
        item.scrub();
        return;
    }
    let _close = ClipboardCloseGuard;
    // SAFETY: clipboard is open on this thread.
    if unsafe { windows::Win32::System::DataExchange::EmptyClipboard() }.is_err() {
        item.scrub();
        return;
    }
    let marker = state
        .suppressor
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .mark_injected(item.sequence);
    let marker_format = state.marker_format.load(Ordering::Acquire);
    if marker.is_none_or(|marker| set_clipboard_bytes(marker_format, &marker.encode()).is_err()) {
        item.scrub();
        return;
    }
    let result = match item.kind {
        ClipboardContentKind::TextUtf8 => {
            let text = std::str::from_utf8(&item.bytes).map_err(|_| "clipboard text is not UTF-8");
            text.and_then(|text| {
                let mut wide = text.encode_utf16().collect::<Vec<_>>();
                wide.push(0);
                let byte_len = wide
                    .len()
                    .checked_mul(2)
                    .ok_or("clipboard UTF-16 size overflow")?;
                let bytes = unsafe {
                    // SAFETY: u16 is plain data and byte_len covers the initialized vector.
                    std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), byte_len)
                };
                set_clipboard_bytes(CF_UNICODETEXT_VALUE, bytes)
            })
        }
        ClipboardContentKind::ImagePng => png_to_dibv5(
            &item.bytes,
            ImageLimits {
                max_encoded_bytes: state.policy.max_bytes,
                ..ImageLimits::default()
            },
        )
        .map_err(|_| "clipboard PNG conversion failed")
        .and_then(|dib| set_clipboard_bytes(CF_DIBV5_VALUE, &dib)),
    };
    if result.is_err() {
        tracing::warn!(
            target: crate::logging::SESSION,
            sequence = item.sequence,
            kind = ?item.kind,
            size = item.bytes.len(),
            reason = "native_write",
            "clipboard injection rejected"
        );
    }
    item.scrub();
}

struct ClipboardCloseGuard;

impl Drop for ClipboardCloseGuard {
    fn drop(&mut self) {
        // SAFETY: every guard is created only after OpenClipboard succeeds.
        let _ = unsafe { windows::Win32::System::DataExchange::CloseClipboard() };
    }
}

fn read_local_item(sequence: u64, negotiation: ClipboardNegotiation) -> Option<ClipboardItem> {
    use windows::Win32::System::DataExchange::IsClipboardFormatAvailable;

    let policy = negotiation.policy();
    if negotiation.allows(ClipboardFlow::HostToClient, ClipboardContentKind::TextUtf8)
        // SAFETY: format availability has no pointer arguments.
        && unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT_VALUE) }.is_ok()
    {
        if let Some(text) = read_unicode_text(policy.max_bytes) {
            let prepared = policy.prepare_text(ClipboardFlow::HostToClient, &text);
            if !prepared.text.is_empty() {
                return ClipboardItem::new(
                    sequence,
                    ClipboardContentKind::TextUtf8,
                    prepared.text.as_bytes().to_vec(),
                    prepared.truncated,
                );
            }
        }
    }
    if negotiation.allows(ClipboardFlow::HostToClient, ClipboardContentKind::ImagePng)
        // SAFETY: format availability has no pointer arguments.
        && unsafe { IsClipboardFormatAvailable(CF_DIBV5_VALUE) }.is_ok()
    {
        let maximum = MAX_DECODED_IMAGE_BYTES.checked_add(124)?;
        let dib = read_clipboard_bytes(CF_DIBV5_VALUE, maximum)?;
        let png = dibv5_to_png(
            &dib,
            ImageLimits {
                max_encoded_bytes: policy.max_bytes,
                ..ImageLimits::default()
            },
        )
        .ok()?;
        return ClipboardItem::new(sequence, ClipboardContentKind::ImagePng, png, false);
    }
    None
}

fn read_unicode_text(maximum_utf8: usize) -> Option<String> {
    let maximum_wide = maximum_utf8.checked_mul(2)?.checked_add(2)?;
    let bytes = read_clipboard_bytes(CF_UNICODETEXT_VALUE, maximum_wide)?;
    decode_strict_utf16(&bytes)
}

fn decode_strict_utf16(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || bytes.len() % 2 != 0 {
        return None;
    }
    let wide = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    let nul = wide.iter().position(|unit| *unit == 0)?;
    if wide[nul..].iter().any(|unit| *unit != 0) {
        return None;
    }
    String::from_utf16(&wide[..nul]).ok()
}

fn read_marker(format: u32) -> Option<EchoMarker> {
    (format != 0)
        .then(|| read_clipboard_bytes(format, arcen_media::clipboard::ECHO_MARKER_BYTES))
        .flatten()
        .and_then(|bytes| EchoMarker::decode(&bytes))
}

fn read_clipboard_bytes(format: u32, maximum: usize) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::GetClipboardData;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    // SAFETY: clipboard is open on this thread; returned HANDLE remains system-owned.
    let handle = unsafe { GetClipboardData(format) }.ok()?;
    let global = HGLOBAL(handle.0);
    // SAFETY: global is the clipboard's movable-memory handle.
    let size = unsafe { GlobalSize(global) };
    if size == 0 || size > maximum {
        return None;
    }
    // SAFETY: global is valid while clipboard remains open.
    let pointer = unsafe { GlobalLock(global) };
    if pointer.is_null() {
        return None;
    }
    let mut bytes = Vec::new();
    if bytes.try_reserve_exact(size).is_err() {
        // SAFETY: balances the successful GlobalLock above.
        let _ = unsafe { GlobalUnlock(global) };
        return None;
    }
    // SAFETY: GlobalSize bounded the readable allocation and the lock pins it.
    bytes.extend_from_slice(unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), size) });
    // SAFETY: balances the successful GlobalLock above; an unlocked result may
    // report ERROR_NOT_LOCKED when the lock count reaches zero, which is benign.
    let _ = unsafe { GlobalUnlock(global) };
    Some(bytes)
}

fn set_clipboard_bytes(format: u32, bytes: &[u8]) -> Result<(), &'static str> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::SetClipboardData;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    // SAFETY: requests a movable allocation with an exact checked slice length.
    let global =
        unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }.map_err(|_| "GlobalAlloc failed")?;
    // SAFETY: global is a fresh movable allocation owned by this function.
    let pointer = unsafe { GlobalLock(global) };
    if pointer.is_null() {
        // SAFETY: ownership has not been transferred.
        let _ = unsafe { GlobalFree(global) };
        return Err("GlobalLock failed");
    }
    // SAFETY: destination is valid for bytes.len() and does not overlap source.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len()) };
    // SAFETY: balances the successful GlobalLock above.
    let _ = unsafe { GlobalUnlock(global) };
    // SAFETY: clipboard is open and takes ownership only on success.
    if unsafe { SetClipboardData(format, HANDLE(global.0)) }.is_err() {
        // SAFETY: SetClipboardData failed, so ownership remains local.
        let _ = unsafe { GlobalFree(global) };
        return Err("SetClipboardData failed");
    }
    Ok(())
}

fn open_clipboard_with_retries<Open, Sleep>(mut open: Open, mut sleep: Sleep) -> bool
where
    Open: FnMut() -> bool,
    Sleep: FnMut(std::time::Duration),
{
    if open() {
        return true;
    }
    for delay in OPEN_RETRY_DELAYS_MS {
        sleep(std::time::Duration::from_millis(delay));
        if open() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ClipboardPolicy {
        ClipboardPolicy::new(
            ClipboardDirection::Both,
            ClipboardContent::All,
            2 * 1024 * 1024,
        )
        .unwrap()
    }

    #[test]
    fn exact_v1_and_capability_bits_gate_negotiation() {
        let mut hello = ClientHelloMsg {
            clipboard_protocol_version: CLIPBOARD_PROTOCOL_VERSION,
            clipboard_text_c2s: true,
            clipboard_text_s2c: false,
            ..ClientHelloMsg::default()
        };
        let negotiated = ClipboardNegotiation::from_client(policy(), &hello).unwrap();
        assert!(negotiated.allows(ClipboardFlow::ClientToHost, ClipboardContentKind::TextUtf8));
        assert!(!negotiated.allows(ClipboardFlow::HostToClient, ClipboardContentKind::TextUtf8));
        hello.clipboard_protocol_version = 0;
        assert!(ClipboardNegotiation::from_client(policy(), &hello).is_none());
    }

    #[tokio::test]
    async fn writer_queue_is_one_active_one_pending_and_chunks_one_at_a_time() {
        let queue = ClipboardWriterQueue::new();
        assert!(queue.enqueue(
            ClipboardItem::new(
                1,
                ClipboardContentKind::TextUtf8,
                vec![b'a'; CHUNK_BYTES + 1],
                false
            )
            .unwrap()
        ));
        assert!(matches!(queue.pop().await.unwrap(), Some(Message::Text(_))));
        assert_eq!(queue.bounded_counts(), (1, 0));
        assert!(queue.enqueue(
            ClipboardItem::new(2, ClipboardContentKind::TextUtf8, b"new".to_vec(), false).unwrap()
        ));
        assert_eq!(queue.bounded_counts(), (1, 1));
        assert!(matches!(queue.pop().await.unwrap(), Some(Message::Text(_))));
        let Some(Message::Binary(frame)) = queue.pop().await.unwrap() else {
            panic!("new transfer chunk");
        };
        assert_eq!(frame.len(), arcen_protocol::CLIPBOARD_HEADER_SIZE + 3);
        assert_eq!(queue.bounded_counts(), (0, 0));
    }

    #[test]
    fn clipboard_open_uses_exact_retry_schedule() {
        let mut attempts = 0;
        let mut delays = Vec::new();
        assert!(!open_clipboard_with_retries(
            || {
                attempts += 1;
                false
            },
            |delay| delays.push(delay.as_millis() as u64)
        ));
        assert_eq!(attempts, 9);
        assert_eq!(delays, OPEN_RETRY_DELAYS_MS);
    }

    #[test]
    fn utf16_is_strict_and_requires_nul() {
        assert_eq!(decode_strict_utf16(&[b'A', 0, 0, 0]), Some("A".to_string()));
        assert!(decode_strict_utf16(&[b'A', 0]).is_none());
        assert!(decode_strict_utf16(&[0, 0, b'B', 0]).is_none());
        assert!(decode_strict_utf16(&[0, 0, 0]).is_none());
        assert!(decode_strict_utf16(&[0x00, 0xd8, 0, 0]).is_none());
    }

    #[test]
    fn policy_message_is_exact_and_never_logs_payload() {
        let message = policy_message(policy());
        assert_eq!(message.protocol_version, CLIPBOARD_PROTOCOL_VERSION);
        let item = ClipboardItem::new(
            3,
            ClipboardContentKind::TextUtf8,
            b"private text".to_vec(),
            false,
        )
        .unwrap();
        assert!(!format!("{item:?}").contains("private text"));
    }
}
