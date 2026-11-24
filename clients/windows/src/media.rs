// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! One connection's media on the Windows Deck.
//!
//! The composition the macOS Deck's `ui/media_worker.rs` makes for
//! `VideoToolbox` and Metal, made here for Windows: Arcen's own D3D11 decoder
//! ([`crate::decode`]) on a device of its own on the window's GPU (pictures
//! reach the presenters through shared textures and fences, so no window's
//! `Present` can stall decode; on a driver that cannot share them the
//! decoder runs on the window's device instead), the two dedicated presenters
//! ([`crate::present::D3d11VideoSink`]) drawing into the window's video
//! visual, and the shared media worker, monitor router and audio worker
//! from `arcen-deck-core`. Everything after the composition (decode order,
//! keyframe recovery, fallbacks, statistics) is the shared worker's.
//!
//! The shell (`ui` stream, [`crate::shell`]) owns the window and the session
//! controller; per connection attempt it hands this module the session
//! ([`SessionHandle`]) and the window's [`VideoSurface`], calls
//! [`MediaSession::poll`] once per UI pass, and drops the
//! [`MediaSession`] when the attempt ends. A reconnect is a new
//! [`MediaSession`]: a fresh worker, decoder and inbox that waits for a
//! fresh keyframe (the macOS Deck's rule).

use std::sync::{Arc, Mutex, PoisonError};

use arcen_deck_core::client_telemetry::ClientTelemetry;
use arcen_deck_core::present::{
    CanvasExecution, CanvasExecutorFacts, CanvasOutcome, CanvasReadback, LayerColourState,
    PresentOutcome, PresentableFrame, PresenterPipeline, PresenterTelemetryDelta, RepaintRequester,
    VideoPresenterSink,
};
use arcen_deck_core::runtime::audio_worker::AudioPlayback;
use arcen_deck_core::runtime::clipboard_session::ClipboardSession;
use arcen_deck_core::runtime::media_worker::{self as worker, MediaPlatform};
use arcen_deck_core::runtime::session::{
    SessionCommand, SessionCommandSender, SessionEvent, SessionHandle,
};
use arcen_media::refine::hub::DeckRefineHub;
use tokio::sync::mpsc;

use crate::decode::{D3d11DecoderFactory, D3d11Picture, D3d11VideoDecoder, VideoDevice};
use crate::present::composition::SharedTree;
use crate::present::{D3d11VideoSink, Gpu};

/// The Windows Deck's media pipeline: D3D11 pictures from Arcen's own
/// decoder, presented by the eight-bit and ten-bit swapchain sinks. The
/// presenters report the shared [`arcen_deck_core::present::PresentOutcome`]
/// directly.
#[derive(Debug, Default)]
pub struct WindowsMedia;

impl MediaPlatform for WindowsMedia {
    type NativeFrame = D3d11Picture;
    type Decoder = D3d11VideoDecoder;
    type EightBitPresenter = WindowSink;
    type TenBitPresenter = WindowSink;
    type EightBitStatus = PresentOutcome;
    type TenBitStatus = PresentOutcome;
}

/// A swapchain sink for one connection: once torn down (the connection
/// ended, or its pipeline fell back) it never presents again, so a frame
/// the worker was already handing over cannot restart a presenter on the
/// window's video visual behind a newer connection.
pub struct WindowSink {
    inner: D3d11VideoSink,
    closed: bool,
}

impl WindowSink {
    fn new(inner: D3d11VideoSink) -> Self {
        Self {
            inner,
            closed: false,
        }
    }
}

impl std::fmt::Debug for WindowSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowSink")
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl VideoPresenterSink for WindowSink {
    type NativeFrame = D3d11Picture;

    const CANVAS_EXECUTOR: Option<CanvasExecutorFacts> =
        <crate::present::D3d11VideoSink as VideoPresenterSink>::CANVAS_EXECUTOR;

    fn execute(&mut self, canvas: &CanvasExecution<'_, '_, D3d11Picture>) -> CanvasOutcome {
        if self.closed {
            return CanvasOutcome::Unsupported;
        }
        self.inner.execute(canvas)
    }

    fn take_canvas_readback(&mut self) -> Option<CanvasReadback> {
        self.inner.take_canvas_readback()
    }

    fn pipeline(&self) -> PresenterPipeline {
        self.inner.pipeline()
    }

    fn present(&mut self, frame: &PresentableFrame<D3d11Picture>) -> PresentOutcome {
        if self.closed {
            return PresentOutcome::Inactive;
        }
        self.inner.present(frame)
    }

    fn represent_last(&mut self) -> Option<PresentOutcome> {
        if self.closed {
            return None;
        }
        self.inner.represent_last()
    }

    fn set_surface_visible(&mut self, visible: bool) {
        self.inner.set_surface_visible(visible);
    }

    fn take_async_status(&mut self) -> Option<PresentOutcome> {
        self.inner.take_async_status()
    }

    fn has_presented_picture(&self) -> bool {
        !self.closed && self.inner.has_presented_picture()
    }

    fn take_telemetry(&self) -> PresenterTelemetryDelta {
        self.inner.take_telemetry()
    }

    fn colour_state(&self) -> Option<LayerColourState> {
        self.inner.colour_state()
    }

    fn teardown(&mut self) {
        self.closed = true;
        self.inner.teardown();
    }
}

/// What the media worker publishes for the window: statistics, the host's
/// prompts and results, cursor and keymap notices.
pub type SharedMediaState = worker::SharedMediaState<WindowsMedia>;

/// Makes one connection's audio player, on the audio worker's thread.
pub type AudioPlayerFactory = Box<dyn FnOnce() -> Box<dyn AudioPlayback> + Send>;

/// The window's video surface, from the shell: the GPU of the adapter that
/// owns the window's monitor and the window's composition tree. The decoder
/// runs on a device of its own on the same adapter, or on this device when
/// the driver cannot share pictures between devices
/// ([`MediaSession::decodes_on_own_device`]).
#[derive(Clone)]
pub struct VideoSurface {
    /// The presenter's device.
    pub gpu: Gpu,
    /// The window's `DirectComposition` tree; the sinks draw into its video
    /// visual.
    pub tree: SharedTree,
}

impl std::fmt::Debug for VideoSurface {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VideoSurface")
            .finish_non_exhaustive()
    }
}

/// Per-attempt inputs besides the session and the surface.
pub struct MediaStart {
    /// The shell's connection generation (stale results are ignored by it).
    pub generation: u64,
    /// A resume attempt: the state starts waiting for a keyframe.
    pub resuming: bool,
    /// The connection's telemetry, the one its `ConnectOptions` carry.
    pub telemetry: Arc<ClientTelemetry>,
    /// The audio player (platform: `PcmAudioPlayer` on the WASAPI output).
    pub audio_player: AudioPlayerFactory,
    /// The picture the connection asks for (its chroma and depth decide
    /// which decoder output formats it may need); `None`: any.
    pub requested_video: Option<arcen_media::VideoConfiguration>,
}

impl std::fmt::Debug for MediaStart {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MediaStart")
            .field("generation", &self.generation)
            .field("resuming", &self.resuming)
            .finish_non_exhaustive()
    }
}

/// One connection attempt's running media: the shared worker on its own
/// thread, the decoder on the window's GPU and the two presenters.
pub struct MediaSession {
    shared: Arc<Mutex<SharedMediaState>>,
    commands: SessionCommandSender,
    events: mpsc::UnboundedSender<SessionEvent>,
    clipboard: ClipboardSession,
    refine: DeckRefineHub,
    eight_bit: Arc<Mutex<WindowSink>>,
    ten_bit: Arc<Mutex<WindowSink>>,
    frames_submitted: u64,
    frames_confirmed: u64,
    /// The connection's telemetry: the presenters' frames count as
    /// presented for health, the session summary and the support bundle.
    telemetry: Arc<ClientTelemetry>,
    /// Whether a presenter has confirmed a frame on glass yet.
    presented_time_reported: bool,
    /// The decoder has a device of its own: pictures can be presented on
    /// any device of the adapter.
    own_decode_device: bool,
    /// Stream Statistics' line about the decoder's device.
    decode_device_line: String,
}

impl std::fmt::Debug for MediaSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MediaSession")
            .finish_non_exhaustive()
    }
}

impl MediaSession {
    /// Starts the media worker for `handle` (from
    /// `arcen_deck_core::runtime::session::spawn_session`, `_opt_in` or
    /// `spawn_resume_session`) on the window's `surface`. Must run inside the
    /// shell's tokio runtime context (the session's tasks are already
    /// spawned there); the worker itself is an OS thread.
    ///
    /// # Errors
    ///
    /// The window's GPU has no D3D11 video device (the session is closed
    /// before returning, so the host sees a clean end).
    pub fn start(
        handle: SessionHandle,
        surface: &VideoSurface,
        repaint: Arc<dyn RepaintRequester>,
        start: MediaStart,
    ) -> Result<Self, String> {
        let SessionHandle {
            events,
            event_sender,
            media,
            commands,
            clipboard,
        } = handle;
        let mut decode_device_line =
            "Decode device: its own (never waits for the display)".to_owned();
        let decoders = match decode_device_of_its_own(&surface.gpu, start.requested_video.as_ref())
        {
            Ok(decoders) => decoders,
            Err(reason) => {
                decode_device_line = format!(
                    "Decode device: the window's ({})",
                    reason.split(';').next().unwrap_or_default()
                );
                let device = match VideoDevice::from_gpu(&surface.gpu) {
                    Ok(device) => device,
                    Err(error) => {
                        let _ = commands.send(SessionCommand::Close);
                        return Err(format!(
                            "this PC's graphics adapter has no D3D11 video decoder: {error}"
                        ));
                    }
                };
                tracing::info!(
                    target: arcen_deck_core::log_target::VIDEO,
                    adapter = %surface.gpu.adapter_name,
                    %reason,
                    "video decode device: the window's device (pictures are not shared between devices)"
                );
                D3d11DecoderFactory::new(device)
            }
        };
        let own_decode_device = decoders.has_own_device();
        let eight_bit = Arc::new(Mutex::new(WindowSink::new(D3d11VideoSink::new(
            PresenterPipeline::EightBit,
            surface.gpu.clone(),
            Arc::clone(&surface.tree),
        ))));
        let ten_bit = Arc::new(Mutex::new(WindowSink::new(D3d11VideoSink::new(
            PresenterPipeline::TenBit,
            surface.gpu.clone(),
            Arc::clone(&surface.tree),
        ))));
        let refine = media.refine().clone();
        {
            let repaint = Arc::clone(&repaint);
            refine.set_waker(Arc::new(move || repaint.request_repaint()));
        }
        let shared = Arc::new(Mutex::new(SharedMediaState::fresh(
            start.generation,
            start.resuming,
        )));
        {
            let mut state = shared.lock().unwrap_or_else(PoisonError::into_inner);
            // The sinks attach to the window's video visual on their first
            // frame, so both pipelines are ready from the start; the Mac
            // sets these once its Metal layers are attached.
            state.eight_bit_video_presenter = Some(Arc::clone(&eight_bit));
            state.eight_bit_layer_ready = true;
            state.ten_bit_video_presenter = Some(Arc::clone(&ten_bit));
            state.ten_bit_layer_ready = true;
            // The root window of a multi-monitor session presents its
            // monitor through the same eight-bit presenter.
            state.routed_primary_dedicated_eight_bit = true;
            state.refine_hub = Some(refine.clone());
        }
        worker::spawn_media_worker(
            events,
            media,
            commands.clone(),
            Arc::clone(&shared),
            repaint,
            Arc::clone(&start.telemetry),
            decoders,
            start.audio_player,
        );
        Ok(Self {
            shared,
            commands,
            events: event_sender,
            clipboard,
            refine,
            eight_bit,
            ten_bit,
            frames_submitted: 0,
            frames_confirmed: 0,
            telemetry: start.telemetry,
            presented_time_reported: false,
            own_decode_device,
            decode_device_line,
        })
    }

    /// Whether the decoder runs on a device of its own. Then every window
    /// presents on a device of its own too; otherwise every window must
    /// present on the root window's device, where the pictures are.
    #[must_use]
    pub const fn decodes_on_own_device(&self) -> bool {
        self.own_decode_device
    }

    /// Stream Statistics' line saying which device the decoder runs on,
    /// and why not its own.
    #[must_use]
    pub fn decode_device_line(&self) -> &str {
        &self.decode_device_line
    }

    /// Frames a further window of a multi-window session presented: they
    /// count for session health as the root window's do.
    pub fn record_secondary_presented(&self, frames: u64) {
        self.telemetry.record_frames_presented(frames);
    }

    /// What the worker publishes (read under the lock, briefly).
    #[must_use]
    pub const fn shared(&self) -> &Arc<Mutex<SharedMediaState>> {
        &self.shared
    }

    /// Sends commands and input to the host.
    #[must_use]
    pub const fn commands(&self) -> &SessionCommandSender {
        &self.commands
    }

    /// The connection's clipboard state (the platform clipboard controller
    /// attaches to it).
    #[must_use]
    pub const fn clipboard(&self) -> &ClipboardSession {
        &self.clipboard
    }

    /// This connection's Exact still image hub.
    #[must_use]
    pub const fn refine(&self) -> &DeckRefineHub {
        &self.refine
    }

    /// Once per UI pass: applies the presenters' late outcomes (a device
    /// loss asks the host for a full frame), records which pipeline has a
    /// picture on glass and the presenters' statistics, copies the audio
    /// worker's status. (When each frame is shown travels with the frame:
    /// `PresentableFrame::timing`.) Returns this pass's presenter
    /// statistics.
    pub fn poll(&mut self) -> worker::PresenterPass {
        if worker::apply_presenter_async_statuses(&self.shared) {
            let _ = self
                .events
                .send(SessionEvent::PresentationRecoveryRequested);
        }
        let pass = worker::drain_presenters(&self.shared);
        let (mut confirmed, mut unconfirmed, mut superseded) = (0_u64, 0_u64, 0_u64);
        for delta in [&pass.eight_bit, &pass.ten_bit] {
            self.frames_submitted = self.frames_submitted.saturating_add(delta.frames_submitted);
            self.frames_confirmed = self.frames_confirmed.saturating_add(delta.frames_confirmed);
            confirmed = confirmed.saturating_add(delta.frames_confirmed);
            unconfirmed = unconfirmed.saturating_add(delta.presented_time_unconfirmed);
            superseded = superseded
                .saturating_add(delta.dropped_overflow)
                .saturating_add(delta.dropped_stale);
        }
        self.telemetry.record_presenter_superseded(superseded);
        // As the macOS Deck does after its presenters' pass.
        self.presented_time_reported |= confirmed > 0;
        self.telemetry.record_presenter_frames(
            confirmed,
            unconfirmed,
            self.presented_time_reported,
        );
        let refine_offered = {
            let mut state = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
            worker::mirror_audio_playback(&mut state);
            refine_offered(&state)
        };
        // Exact still image on a still screen (the macOS Deck's
        // `drive_exact_still_image`): no new video frame arrives, so the
        // tiles the hub admitted reach glass, and are read back and
        // verified, only when the last ten-bit frame is presented again.
        if refine_offered
            && self
                .refine
                .overlay_dirty(arcen_media::refine::PRIMARY_MONITOR)
        {
            let _ = self
                .ten_bit
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .represent_last();
        }
        pass
    }

    /// Whether a picture of this connection is on the window now (a
    /// presenter thread confirmed a present, not only a hand-over).
    #[must_use]
    pub fn has_presented_picture(&self) -> bool {
        [&self.eight_bit, &self.ten_bit].iter().any(|sink| {
            sink.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .has_presented_picture()
        })
    }

    /// Presents each pipeline's last frame again, for a pipeline that has
    /// presented one: another connection's presenter of the same pipeline
    /// released the window's video visual (a resumed session replacing the
    /// picture held under the reconnect overlay).
    pub fn represent_last(&self) {
        for sink in [&self.eight_bit, &self.ten_bit] {
            let _ = sink
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .represent_last();
        }
    }

    /// Frames the presenters submitted and confirmed on glass, over the
    /// connection so far (as of the last [`Self::poll`]).
    #[must_use]
    pub const fn presented_frames(&self) -> (u64, u64) {
        (self.frames_submitted, self.frames_confirmed)
    }

    /// The window was minimised, hidden or restored.
    pub fn set_visible(&self, visible: bool) {
        for sink in [&self.eight_bit, &self.ten_bit] {
            sink.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .set_surface_visible(visible);
        }
    }

    /// The worker has stopped (the session ended or failed); the reason is
    /// in the shared state's `end`/`error`.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed
    }

    /// Ends the connection (the user disconnected); dropping the session
    /// releases the presenters.
    pub fn close(&self) {
        let _ = self.commands.send(SessionCommand::Close);
    }
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        // The connection ends with its session object (the macOS Deck
        // closes before it tears down), the worker stops routing to the
        // presenters, and the presenters close: queued frames are
        // discarded, never shown late, and the video visual is cleared.
        let _ = self.commands.send(SessionCommand::Close);
        {
            let mut state = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
            state.eight_bit_video_presenter = None;
            state.eight_bit_layer_disabled = true;
            state.ten_bit_video_presenter = None;
            state.ten_bit_layer_disabled = true;
        }
        for sink in [&self.eight_bit, &self.ten_bit] {
            sink.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .teardown();
        }
    }
}

/// A decoder factory on a D3D11 device of its own on `gpu`'s adapter, when
/// the driver can share decoded pictures with `gpu`'s device (and so with
/// any other device on the adapter).
fn decode_device_of_its_own(
    gpu: &Gpu,
    requested: Option<&arcen_media::VideoConfiguration>,
) -> Result<D3d11DecoderFactory, String> {
    let device = VideoDevice::own_for(gpu)?;
    let proof = crate::decode::prove_sharing(&device, &gpu.device, &gpu.context_lock, gpu.luid)?;
    // Any 4:2:0 picture the adapter decodes may arrive (a fidelity request
    // can be served as Auto); 4:4:4 only when the connection asks for it.
    let full_chroma =
        requested.is_none_or(|video| video.chroma == arcen_media::ChromaSubsampling::Yuv444);
    if let Some((format, why)) = proof.refused.iter().find(|(format, _)| {
        full_chroma
            || !matches!(
                *format,
                windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_Y410
                    | windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_AYUV
            )
    }) {
        return Err(format!(
            "{} pictures this connection may need are not shared exactly ({why}); {}",
            crate::decode::shared::format_name(*format),
            proof.describe()
        ));
    }
    tracing::info!(
        target: arcen_deck_core::log_target::VIDEO,
        adapter = %device.adapter_name,
        proof = %proof.describe(),
        "video decode device: a device of its own, pictures shared with the windows' devices"
    );
    Ok(D3d11DecoderFactory::own_device(
        device,
        proof.formats.into(),
    ))
}

/// Whether the host's hello turned Exact still image on for this session.
fn refine_offered(state: &SharedMediaState) -> bool {
    state
        .server_hello
        .as_ref()
        .and_then(|hello| hello.lossless_refine.as_ref())
        .is_some_and(arcen_protocol::refine::LosslessRefineStatusMsg::is_on)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "a test fails loudly on a missing device or window"
)]
mod tests {
    use super::*;

    fn assert_send<T: Send>() {}

    /// The composition satisfies the shared worker's bounds: the decoder
    /// factory and the presenters cross into the worker thread, and the
    /// state the window reads is shared across threads.
    #[test]
    fn the_windows_media_platform_meets_the_shared_worker_bounds() {
        assert_send::<D3d11DecoderFactory>();
        assert_send::<WindowSink>();
        assert_send::<D3d11Picture>();
        assert_send::<Arc<Mutex<SharedMediaState>>>();
    }

    /// A hidden `WS_EX_NOREDIRECTIONBITMAP` window, as the shell's.
    #[allow(unsafe_code, reason = "Win32 window creation for the test surface")]
    fn hidden_window() -> windows::Win32::Foundation::HWND {
        use windows::Win32::Foundation::{HINSTANCE, HWND};
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, HMENU, WS_EX_NOREDIRECTIONBITMAP, WS_POPUP,
        };
        unsafe {
            CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP,
                windows::core::w!("STATIC"),
                windows::core::w!("arcen media wiring test"),
                WS_POPUP,
                0,
                0,
                640,
                360,
                HWND::default(),
                HMENU::default(),
                HINSTANCE::default(),
                None,
            )
        }
        .expect("hidden test window")
    }

    fn options_for_a_closed_port() -> arcen_deck_core::runtime::session::ConnectOptions {
        arcen_deck_core::runtime::session::ConnectOptions {
            display_refresh: None,
            services: Arc::new(arcen_deck_core::services::test_support::UnavailableServices),
            host: "127.0.0.1".to_owned(),
            port: 9,
            use_tls: true,
            username: String::new(),
            password: String::new(),
            timeout: std::time::Duration::from_secs(2),
            tls: arcen_deck_core::runtime::tls::TlsTrustConfig::system_ca(),
            profile: arcen_deck_core::runtime::session::StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: arcen_protocol::messages::CursorMode::Local,
            clipboard_enabled: false,
            microphone_enabled: false,
            tablet_input_enabled: false,
            tablet_mode_requested: arcen_protocol::messages::TabletModeMsg::LocalTermination,
            telemetry: Arc::new(ClientTelemetry::default()),
            exact_still_image: false,
            canvas: arcen_deck_core::canvas::CanvasRequest::Unavailable,
        }
    }

    struct CountingRepaint(std::sync::atomic::AtomicUsize);

    impl RepaintRequester for CountingRepaint {
        fn request_repaint(&self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// R01 wiring: a session, the D3D11 decoder factory on the window's GPU,
    /// both sinks on the window's composition tree and the shared worker
    /// start together, and a failed connection ends the worker cleanly with
    /// its reason, waking the window. Runs on a Windows machine with a
    /// D3D11 video device (the Windows lab); a GPU without one is refused
    /// with a reason and the session closed.
    #[test]
    fn a_session_starts_its_media_on_the_window_and_ends_cleanly() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _context = runtime.enter();
        let gpu = Gpu::create(&crate::present::AdapterChoice::HighPerformance)
            .or_else(|_| Gpu::create(&crate::present::AdapterChoice::Warp))
            .expect("a D3D11 device");
        let tree = match crate::present::composition::CompositionTree::new(&gpu, hidden_window()) {
            Ok(tree) => tree,
            // DirectComposition needs an interactive desktop: over SSH
            // (session 0) it is refused, and `show` on the console proves
            // this path instead (core/lab/show98-console.sh).
            Err(error) if error.contains("0x80070005") => {
                eprintln!("skipped: no interactive desktop for DirectComposition ({error})");
                return;
            }
            Err(error) => panic!("composition tree: {error}"),
        };
        let surface = VideoSurface {
            gpu,
            tree: Arc::new(Mutex::new(tree)),
        };
        let repaint = Arc::new(CountingRepaint(std::sync::atomic::AtomicUsize::new(0)));
        let handle = arcen_deck_core::runtime::session::spawn_session(options_for_a_closed_port());
        let start = MediaStart {
            generation: 7,
            resuming: false,
            telemetry: Arc::new(ClientTelemetry::default()),
            audio_player: Box::new(|| {
                Box::new(arcen_deck_core::runtime::audio_player::PcmAudioPlayer::<
                    SilentOutput,
                >::awaiting_stream_result())
            }),
            requested_video: None,
        };
        let mut session = match MediaSession::start(
            handle,
            &surface,
            Arc::clone(&repaint) as Arc<dyn RepaintRequester>,
            start,
        ) {
            Ok(session) => session,
            Err(reason) => {
                assert!(reason.contains("video decoder"), "{reason}");
                return;
            }
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !session.is_closed() {
            assert!(
                std::time::Instant::now() < deadline,
                "the worker must end with the session"
            );
            session.poll();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        {
            let state = session
                .shared()
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            assert_eq!(state.generation, 7);
            assert!(state.error.is_some(), "a refused connection says why");
            assert!(state.eight_bit_video_presenter.is_some());
            assert!(state.ten_bit_video_presenter.is_some());
        }
        assert!(repaint.0.load(std::sync::atomic::Ordering::Relaxed) > 0);
        drop(session);
    }

    /// An output device that plays nothing (no audio reaches it here).
    struct SilentOutput;

    impl arcen_deck_core::runtime::audio_player::AudioOutputDevice for SilentOutput {
        fn open() -> Result<Self, String> {
            Ok(Self)
        }

        fn backend(&self) -> &'static str {
            "test-silent"
        }

        fn push_s16le(&self, payload: &[u8]) -> usize {
            payload.len()
        }

        fn push_samples(&self, samples: &[i16]) -> usize {
            samples.len() * 2
        }

        fn reset(&self) {}

        fn snapshot(&self) -> arcen_deck_core::runtime::audio_player::PlaybackSnapshot {
            arcen_deck_core::runtime::audio_player::PlaybackSnapshot::default()
        }
    }
}
