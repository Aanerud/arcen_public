// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Lab harness for the Windows Deck's platform services, run in a console
//! session on a Windows test machine (the UI-thread and device parts cannot
//! be proven by unit tests). Each subcommand prints `key=value` evidence and
//! exits non-zero when it proves nothing usable.
//!
//! ```text
//! platform_lab facts [host port]   build, OS, folders, zone, displays, pen, netinfo
//! platform_lab clipboard           text/PNG/DIB round trips, echo-free writes, listener
//! platform_lab tone SECONDS        play a tone through the output, measure WASAPI loopback
//! platform_lab loopback SECONDS    measure WASAPI loopback only (what is playing now)
//! platform_lab microphone SECONDS  capture through DeckServices::start_microphone
//! platform_lab logs                start the log runtime, write CLIENT_START/STOP
//! platform_lab crash               register WER LocalDumps for this exe, then crash
//! platform_lab session HOST PORT CERT_SHA256 SECONDS [--microphone | --microphone-loopback] [--clip-text TEXT] [--expect-audio]
//!                                  a real Pier session through deck-core's engine:
//!                                  host audio played and heard on loopback, the
//!                                  shared clipboard controller, the microphone;
//!                                  user name and password on stdin (two lines)
//! ```

#[cfg(windows)]
#[allow(
    unsafe_code,
    clippy::print_stdout,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    reason = "a lab harness"
)]
mod lab {
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use arcen_deck_core::DeckServices;
    use arcen_deck_core::clipboard::PlatformClipboard;
    use arcen_deck_core::microphone::{
        CapturedMicrophoneFrame, MicrophoneCancel, MicrophoneCaptureError, MicrophoneFrameSink,
    };
    use arcen_deck_windows::audio::microphone::microphone_privacy;
    use arcen_deck_windows::audio::output::{WasapiOutput, WindowsAudioOutput};
    use arcen_deck_windows::clipboard::{WindowsClipboard, encode_clipboard_utf16};
    use arcen_deck_windows::platform::WindowsDeckServices;
    use arcen_protocol::messages::ClipboardContentKind;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    pub(crate) fn run(args: &[String]) -> i32 {
        match args.first().map(String::as_str) {
            Some("facts") => facts(args.get(1).map(String::as_str), args.get(2)),
            Some("clipboard") => clipboard(),
            Some("tone") => tone(seconds(args), true),
            Some("loopback") => tone(seconds(args), false),
            Some("microphone") => microphone(
                seconds(args),
                args.get(2).map(String::as_str) == Some("loopback"),
            ),
            Some("logs") => logs(),
            Some("crash") => crash(),
            Some("session") => session(&args[1..]),
            _ => {
                eprintln!(
                    "usage: platform_lab facts|clipboard|tone|loopback|microphone|logs|crash|session"
                );
                2
            }
        }
    }

    fn seconds(args: &[String]) -> u64 {
        args.get(1)
            .and_then(|value| value.parse().ok())
            .unwrap_or(5)
    }

    fn facts(host: Option<&str>, port: Option<&String>) -> i32 {
        let services = WindowsDeckServices::new();
        print!(
            "{}",
            arcen_deck_windows::platform::build::version_text(&services.build_identity())
        );
        println!("platform_summary={}", services.platform_summary());
        let dirs = services.directories();
        println!("config_dir={:?}", dirs.config);
        println!("log_dir={:?}", dirs.logs);
        println!("timezone={:?}", services.timezone());
        println!("tablet_presence={:?}", services.tablet_presence());
        println!(
            "pointer_devices={:?}",
            arcen_deck_windows::platform::pen::pointer_device_types()
                .map(|kinds| kinds.iter().map(|kind| kind.0).collect::<Vec<_>>())
        );
        println!("captures_typed_pen={}", services.captures_typed_pen());
        println!("decode={:?}", services.decode_capabilities());
        println!("microphone_privacy={:?}", microphone_privacy());
        for display in arcen_deck_windows::display::facts() {
            println!("display_facts={display:?}");
            if let Some(dxgi) = display.dxgi {
                println!(
                    "display_colour id={} bits_per_color={} colour_space={} hdr={} sdr_white_nits={:?} max_luminance={} min_luminance={} max_full_frame={} advanced_color_supported={:?} enabled={:?} link_bits={:?}",
                    display.id,
                    dxgi.bits_per_color,
                    dxgi.color_space_name(),
                    dxgi.is_hdr(),
                    display.sdr_white_nits,
                    dxgi.max_luminance,
                    dxgi.min_luminance,
                    dxgi.max_full_frame_luminance,
                    display.advanced_color_supported,
                    display.advanced_color_enabled,
                    display.bits_per_color_channel,
                );
            }
        }
        let roster = services.displays();
        for monitor in &roster {
            println!("client_monitor={monitor:?}");
        }
        println!("any_display_is_hdr={}", services.any_display_is_hdr());
        if let (Some(host), Some(port)) = (host, port.and_then(|port| port.parse::<u16>().ok())) {
            let probe = services.probe_network(host, port);
            println!("network_snapshot={:?}", probe.snapshot);
            if let Ok(Some(peer)) = std::net::ToSocketAddrs::to_socket_addrs(&(host, port))
                .map(|mut addresses| addresses.next())
            {
                println!(
                    "outgoing_interface={:?}",
                    services.outgoing_interface(peer.ip())
                );
            }
        }
        i32::from(roster.is_empty())
    }

    // --- Clipboard ----------------------------------------------------------

    fn pump_messages() {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
        };
        let mut message = MSG::default();
        // SAFETY: a plain thread message pump.
        while unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            // SAFETY: as above.
            unsafe {
                let _ = TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
    }

    /// Another application writing the clipboard: another thread, no owner
    /// window, no Arcen marker.
    fn foreign_write(format: u32, bytes: Vec<u8>) -> bool {
        std::thread::spawn(move || {
            use windows::Win32::Foundation::HANDLE;
            use windows::Win32::System::DataExchange::{
                CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
            };
            use windows::Win32::System::Memory::{
                GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
            };
            for _ in 0..20 {
                // SAFETY: lab code; a foreign write with a null owner.
                if unsafe { OpenClipboard(None) }.is_ok() {
                    // SAFETY: the clipboard is open on this thread.
                    unsafe {
                        let _ = EmptyClipboard();
                        let global = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).expect("alloc");
                        let pointer = GlobalLock(global);
                        std::ptr::copy_nonoverlapping(
                            bytes.as_ptr(),
                            pointer.cast::<u8>(),
                            bytes.len(),
                        );
                        let _ = GlobalUnlock(global);
                        let ok = SetClipboardData(format, HANDLE(global.0)).is_ok();
                        let _ = CloseClipboard();
                        return ok;
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        })
        .join()
        .unwrap_or(false)
    }

    fn check(name: &str, ok: bool, failures: &mut u32) {
        println!("{name}={}", if ok { "ok" } else { "FAIL" });
        if !ok {
            *failures += 1;
        }
    }

    fn sample_png() -> Vec<u8> {
        // 3x2 RGBA with a translucent pixel, made by the shared converter.
        let mut dib = vec![0_u8; 40];
        dib[0] = 40;
        dib[4..8].copy_from_slice(&3_i32.to_le_bytes());
        dib[8..12].copy_from_slice(&2_i32.to_le_bytes());
        dib[12] = 1;
        dib[14] = 32;
        for pixel in 0..6_u8 {
            dib.extend_from_slice(&[
                pixel * 40,
                255 - pixel * 40,
                128,
                if pixel == 3 { 128 } else { 255 },
            ]);
        }
        arcen_media::clipboard::dib_to_png(&dib, arcen_media::clipboard::ImageLimits::default())
            .expect("sample PNG")
    }

    fn clipboard() -> i32 {
        let mut failures = 0;
        let Some(mut clipboard) = WindowsClipboard::open() else {
            println!("clipboard_open=FAIL");
            return 1;
        };
        let wakes = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&wakes);
        clipboard.set_change_waker(Box::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }));
        let marker = [7_u8; arcen_media::clipboard::ECHO_MARKER_BYTES];

        // 1. Our own text write: readable, marked, no change for the controller.
        let before = clipboard.change_token();
        let text = "Arcen ✓ 日本語 🚀 Ærø";
        check(
            "write_text",
            clipboard.write(&marker, ClipboardContentKind::TextUtf8, text.as_bytes()),
            &mut failures,
        );
        check(
            "own_write_keeps_token",
            clipboard.change_token() == before,
            &mut failures,
        );
        check(
            "read_text_unicode",
            clipboard.read_text().as_deref().map(String::as_str) == Some(text),
            &mut failures,
        );
        check(
            "origin_marker_present",
            clipboard.read_origin_marker().as_deref() == Some(&marker[..]),
            &mut failures,
        );

        // 2. Another application's text: token moves, no marker, listener fires.
        pump_messages();
        let _ = clipboard.take_changed();
        let foreign = "copied elsewhere — ünïcode";
        check(
            "foreign_text_write",
            foreign_write(13, encode_clipboard_utf16(foreign)),
            &mut failures,
        );
        std::thread::sleep(Duration::from_millis(100));
        pump_messages();
        check("listener_flag", clipboard.take_changed(), &mut failures);
        check(
            "listener_waker",
            wakes.load(Ordering::Relaxed) > 0,
            &mut failures,
        );
        check(
            "foreign_moves_token",
            clipboard.change_token() != before,
            &mut failures,
        );
        check(
            "foreign_has_no_marker",
            clipboard.read_origin_marker().is_none(),
            &mut failures,
        );
        check(
            "read_foreign_text",
            clipboard.read_text().as_deref().map(String::as_str) == Some(foreign),
            &mut failures,
        );

        // 3. Our PNG write: PNG back byte-exact, and a bitmap for Paint.
        let png = sample_png();
        let token = clipboard.change_token();
        check(
            "write_png",
            clipboard.write(&marker, ClipboardContentKind::ImagePng, &png),
            &mut failures,
        );
        check(
            "png_write_keeps_token",
            clipboard.change_token() == token,
            &mut failures,
        );
        check(
            "read_png_exact",
            clipboard.read_png(1 << 20).as_deref() == Some(&png[..]),
            &mut failures,
        );
        let dib_offered = {
            use windows::Win32::System::DataExchange::IsClipboardFormatAvailable;
            // SAFETY: no pointers.
            unsafe { IsClipboardFormatAvailable(8) }.is_ok()
                && unsafe { IsClipboardFormatAvailable(17) }.is_ok()
        };
        check("bitmap_offered_for_paint", dib_offered, &mut failures);

        // 4. Another application's 24-bit DIB only (no PNG): converted.
        let mut dib = vec![0_u8; 40];
        dib[0] = 40;
        dib[4..8].copy_from_slice(&2_i32.to_le_bytes());
        dib[8..12].copy_from_slice(&2_i32.to_le_bytes());
        dib[12] = 1;
        dib[14] = 24;
        dib.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);
        dib.extend_from_slice(&[255, 0, 0, 255, 255, 255, 0, 0]);
        check("foreign_dib_write", foreign_write(8, dib), &mut failures);
        let converted = clipboard.read_png(1 << 20);
        let decoded_ok = converted.as_ref().is_some_and(|png| {
            arcen_media::clipboard::validate_png(
                png,
                arcen_media::clipboard::ImageLimits::default(),
            )
            .is_ok_and(|info| info.width == 2 && info.height == 2)
        });
        check("foreign_dib_to_png", decoded_ok, &mut failures);
        println!("clipboard_failures={failures}");
        i32::from(failures != 0)
    }

    // --- Audio --------------------------------------------------------------

    /// WASAPI loopback of the default output device (cpal opens an input
    /// stream on a render device in loopback mode).
    struct Loopback {
        _stream: cpal::Stream,
        peak: Arc<AtomicI32>,
        sums: Arc<Mutex<(f64, u64)>>,
        format: String,
    }

    fn start_loopback() -> Option<Loopback> {
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        let config = device.default_output_config().ok()?;
        let format = format!(
            "{} Hz {} ch {:?} on {}",
            config.sample_rate().0,
            config.channels(),
            config.sample_format(),
            device.name().unwrap_or_default()
        );
        let peak = Arc::new(AtomicI32::new(0));
        let sums = Arc::new(Mutex::new((0.0_f64, 0_u64)));
        let (peak_cb, sums_cb) = (Arc::clone(&peak), Arc::clone(&sums));
        let stream = device
            .build_input_stream(
                &config.config(),
                move |data: &[f32], _| {
                    let mut local_peak = 0_i32;
                    let mut local_sum = 0.0_f64;
                    for sample in data {
                        local_peak = local_peak.max((sample.abs() * 32767.0) as i32);
                        local_sum += f64::from(*sample) * f64::from(*sample);
                    }
                    peak_cb.fetch_max(local_peak, Ordering::Relaxed);
                    let mut sums = sums_cb.lock().expect("sums");
                    sums.0 += local_sum;
                    sums.1 += data.len() as u64;
                },
                |error| eprintln!("loopback error: {error}"),
                None,
            )
            .ok()?;
        stream.play().ok()?;
        Some(Loopback {
            _stream: stream,
            peak,
            sums,
            format,
        })
    }

    fn loopback_report(loopback: &Loopback, label: &str) -> (i32, f64) {
        let peak = loopback.peak.swap(0, Ordering::Relaxed);
        let mut sums = loopback.sums.lock().expect("sums");
        let rms = if sums.1 == 0 {
            0.0
        } else {
            (sums.0 / sums.1 as f64).sqrt()
        };
        println!(
            "{label}_loopback_peak={peak} {label}_loopback_rms={rms:.5} {label}_loopback_samples={}",
            sums.1
        );
        *sums = (0.0, 0);
        (peak, rms)
    }

    fn tone(seconds: u64, play: bool) -> i32 {
        let Some(loopback) = start_loopback() else {
            println!("loopback=FAIL (no default output device)");
            return 1;
        };
        println!("loopback_format={}", loopback.format);
        std::thread::sleep(Duration::from_millis(500));
        let _ = loopback_report(&loopback, "baseline");
        if !play {
            std::thread::sleep(Duration::from_secs(seconds));
            let (peak, _) = loopback_report(&loopback, "listen");
            return i32::from(peak == 0);
        }
        let output = match WindowsAudioOutput::start(WasapiOutput::default) {
            Ok(output) => output,
            Err(error) => {
                println!("output=FAIL {error}");
                return 1;
            }
        };
        println!(
            "output_device={} output_format={} backend={}",
            output.status().device,
            output.status().format,
            arcen_deck_windows::audio::output::BACKEND
        );
        // 440 Hz at -12 dBFS, fed in 20 ms frames like the audio lane.
        let started = Instant::now();
        let mut phase = 0.0_f64;
        let mut fed_frames = 0_u64;
        while started.elapsed() < Duration::from_secs(seconds) {
            let mut frame = [0_i16; 1920];
            for pair in frame.chunks_exact_mut(2) {
                let value = (phase.sin() * 8_192.0) as i16;
                pair[0] = value;
                pair[1] = value;
                phase += 2.0 * std::f64::consts::PI * 440.0 / 48_000.0;
            }
            let _ = output.push_samples(&frame);
            fed_frames += 1;
            let due = Duration::from_millis(20 * fed_frames);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        let snapshot = output.snapshot();
        println!(
            "playout queued_ms={} underruns={} trims={} phase={} device_changes={}",
            snapshot.queued_ms,
            snapshot.underruns,
            snapshot.trim_events,
            snapshot.phase(),
            output.status().rebuilds
        );
        let (peak, rms) = loopback_report(&loopback, "tone");
        drop(output);
        let ok = peak > 1_000 && rms > 0.02;
        println!("tone_heard={}", if ok { "ok" } else { "FAIL" });
        i32::from(!ok)
    }

    // --- Microphone ---------------------------------------------------------

    #[derive(Default)]
    struct CountingSink {
        frames: AtomicU64,
        peak: AtomicI32,
        last_sequence: AtomicU64,
        finished: Mutex<Option<Result<(), MicrophoneCaptureError>>>,
    }

    impl MicrophoneFrameSink for CountingSink {
        fn push(&self, frame: CapturedMicrophoneFrame) {
            self.frames.fetch_add(1, Ordering::Relaxed);
            let peak = frame
                .samples
                .iter()
                .map(|sample| i32::from(*sample).abs())
                .max()
                .unwrap_or(0);
            self.peak.fetch_max(peak, Ordering::Relaxed);
            self.last_sequence
                .store(u64::from(frame.sequence), Ordering::Relaxed);
        }

        fn finish(&self, result: Result<(), MicrophoneCaptureError>) {
            *self.finished.lock().expect("finish") = Some(result);
        }
    }

    /// Plays a 440 Hz tone through the Deck output until `stop`.
    fn play_tone_until(stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let Ok(output) = WindowsAudioOutput::start(WasapiOutput::default) else {
                return;
            };
            let started = Instant::now();
            let mut phase = 0.0_f64;
            let mut fed = 0_u64;
            while !stop.load(Ordering::Acquire) {
                let mut frame = [0_i16; 1920];
                for pair in frame.chunks_exact_mut(2) {
                    let value = (phase.sin() * 8_192.0) as i16;
                    pair[0] = value;
                    pair[1] = value;
                    phase += 2.0 * std::f64::consts::PI * 440.0 / 48_000.0;
                }
                let _ = output.push_samples(&frame);
                fed += 1;
                if let Some(wait) = Duration::from_millis(20 * fed).checked_sub(started.elapsed()) {
                    std::thread::sleep(wait);
                }
            }
        })
    }

    fn microphone(seconds: u64, loopback: bool) -> i32 {
        println!("microphone_privacy={:?}", microphone_privacy());
        let host = cpal::default_host();
        println!(
            "default_input={:?}",
            host.default_input_device()
                .and_then(|device| device.name().ok())
        );
        if let Ok(devices) = host.input_devices() {
            for device in devices {
                println!(
                    "input_device={:?} config={:?}",
                    device.name().ok(),
                    device.default_input_config().ok()
                );
            }
        }
        let services = WindowsDeckServices::new();
        let sink = Arc::new(CountingSink::default());
        let cancel = MicrophoneCancel {
            stop: Arc::new(AtomicBool::new(false)),
            session_closed: Arc::new(AtomicBool::new(false)),
        };
        let tone_stop = Arc::new(AtomicBool::new(false));
        let tone = loopback.then(|| play_tone_until(Arc::clone(&tone_stop)));
        if loopback {
            std::thread::sleep(Duration::from_millis(500));
        }
        let started = Instant::now();
        let result = if loopback {
            arcen_deck_windows::audio::microphone::start_from(
                arcen_deck_windows::audio::microphone::CaptureSource::RenderLoopback,
                sink.clone(),
                &cancel,
            )
            .map(|stream| {
                Box::new(stream) as Box<dyn arcen_deck_core::microphone::MicrophoneStream>
            })
        } else {
            services.start_microphone(sink.clone(), cancel)
        };
        let mut stream = match result {
            Ok(stream) => stream,
            Err(error) => {
                println!("microphone_start={error:?}");
                return 1;
            }
        };
        println!("microphone_started_ms={}", started.elapsed().as_millis());
        std::thread::sleep(Duration::from_secs(seconds));
        stream.stop();
        tone_stop.store(true, Ordering::Release);
        if let Some(tone) = tone {
            let _ = tone.join();
        }
        let frames = sink.frames.load(Ordering::Relaxed);
        println!(
            "microphone_frames={frames} expected~{} peak={} last_sequence={} finished={:?}",
            seconds * 50,
            sink.peak.load(Ordering::Relaxed),
            sink.last_sequence.load(Ordering::Relaxed),
            sink.finished.lock().expect("finish")
        );
        i32::from(frames < seconds * 40)
    }

    // --- Logs and crash dumps -----------------------------------------------

    fn logs() -> i32 {
        let services = WindowsDeckServices::new();
        let profile = arcen_telemetry::OperationalProfile::Info;
        let dir = arcen_deck_windows::logging::init(&services, profile);
        println!("log_dir={dir:?}");
        tracing::info!(target: "arcen::ui", "platform lab log line");
        arcen_deck_core::logging::shutdown();
        let Some(dir) = dir else {
            return 1;
        };
        let files: Vec<_> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default();
        for file in &files {
            let text = std::fs::read_to_string(file).unwrap_or_default();
            println!(
                "log_file={} lines={} client_start={} client_stop={}",
                file.display(),
                text.lines().count(),
                text.contains("CLIENT_START"),
                text.contains("CLIENT_STOP")
            );
        }
        i32::from(files.is_empty())
    }

    fn crash() -> i32 {
        let exe = std::env::current_exe().expect("exe");
        let name = exe
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        match arcen_deck_windows::logging::register_crash_dumps(&name) {
            Ok(()) => println!(
                "wer_registered={name} folder={:?}",
                arcen_deck_windows::logging::crash_dump_dir()
            ),
            Err(code) => {
                println!("wer_registered=FAIL {code}");
                return 1;
            }
        }
        println!("crashing now");
        // SAFETY: none; a deliberate access violation for WER (lab only).
        unsafe { std::ptr::null_mut::<u32>().write_volatile(0xDEAD) };
        0
    }

    // --- A real Pier session ------------------------------------------------

    fn flag_value(args: &[String], name: &str) -> Option<String> {
        args.iter()
            .position(|arg| arg == name)
            .and_then(|index| args.get(index + 1).cloned())
    }

    /// Host audio from the audio lane, decoded and played through the
    /// Deck's WASAPI output.
    /// The last feed status of the product player, for progress lines.
    type FeedSnapshot = Arc<Mutex<Option<arcen_deck_core::runtime::audio_worker::AudioFeedStatus>>>;

    /// Host audio from the audio lane through the product's player
    /// (`PcmAudioPlayer<WindowsAudioOutput>`: the Mac's stream result,
    /// Opus, jitter and PLC logic on WASAPI). A second decoder only
    /// measures the decoded peak, so silence on the wire is told apart from
    /// silence at the device.
    fn spawn_audio_player(
        lane: arcen_deck_core::runtime::audio_lane::AudioLaneReceiver,
        stop: Arc<AtomicBool>,
        counts: Arc<(AtomicU64, AtomicU64, AtomicI32)>,
        last: FeedSnapshot,
    ) -> std::thread::JoinHandle<()> {
        use arcen_deck_core::runtime::audio_lane::AudioLaneItem;
        std::thread::spawn(move || {
            let mut player = arcen_deck_windows::audio::output::Player::awaiting_stream_result();
            let mut measure = arcen_media::audio::OpusDecoder::new().ok();
            let samples = arcen_media::audio::AudioFrameSpec::V1
                .interleaved_samples()
                .unwrap_or(1920);
            let mut pcm = vec![0_i16; samples];
            while !stop.load(Ordering::Acquire) {
                let Some(items) = lane.recv_batch(Duration::from_millis(100)) else {
                    continue;
                };
                for item in items {
                    match item {
                        AudioLaneItem::StreamResult(result) => {
                            println!(
                                "audio_stream_result enabled={} reason={:?} config={:?}",
                                result.enabled, result.reason, result.config
                            );
                            player.set_stream_result(&result);
                        }
                        AudioLaneItem::Frame {
                            header, payload, ..
                        } => {
                            counts.0.fetch_add(1, Ordering::Relaxed);
                            let peak = match header.codec {
                                arcen_protocol::AudioCodec::Opus => measure
                                    .as_mut()
                                    .and_then(|decoder| decoder.decode(&payload, &mut pcm).ok())
                                    .map(|()| {
                                        pcm.iter()
                                            .map(|sample| i32::from(*sample).abs())
                                            .max()
                                            .unwrap_or(0)
                                    }),
                                arcen_protocol::AudioCodec::Pcm => payload
                                    .chunks_exact(2)
                                    .map(|bytes| {
                                        i32::from(i16::from_le_bytes([bytes[0], bytes[1]])).abs()
                                    })
                                    .max(),
                            };
                            if let Some(peak) = peak {
                                counts.2.fetch_max(peak, Ordering::Relaxed);
                            }
                            let status = player.feed(header, &payload);
                            if status.accepted_bytes > 0 {
                                counts.1.fetch_add(1, Ordering::Relaxed);
                            }
                            *last.lock().expect("feed status") = Some(status);
                        }
                    }
                }
            }
        })
    }

    /// The Deck's services with the lab-only loopback microphone source
    /// (`CaptureSource::RenderLoopback`) in place of the default input.
    struct LoopbackMicrophone(Arc<WindowsDeckServices>);

    impl DeckServices for LoopbackMicrophone {
        fn build_identity(&self) -> arcen_protocol::messages::BuildIdentityMsg {
            self.0.build_identity()
        }
        fn platform_summary(&self) -> String {
            self.0.platform_summary()
        }
        fn native_config_dir(&self) -> Option<std::path::PathBuf> {
            self.0.native_config_dir()
        }
        fn native_log_dir(&self) -> Option<std::path::PathBuf> {
            self.0.native_log_dir()
        }
        fn decode_capabilities(&self) -> arcen_deck_core::decode::DecodeCapabilities {
            self.0.decode_capabilities()
        }
        fn displays(&self) -> Vec<arcen_protocol::messages::ClientMonitor> {
            self.0.displays()
        }
        fn display_color(&self, id: u32) -> Option<arcen_protocol::messages::DisplayColorMsg> {
            self.0.display_color(id)
        }
        fn captures_typed_pen(&self) -> bool {
            self.0.captures_typed_pen()
        }
        fn tablet_presence(&self) -> arcen_input::CapabilityAvailability {
            self.0.tablet_presence()
        }
        fn open_clipboard(&self) -> Option<Box<dyn PlatformClipboard>> {
            self.0.open_clipboard()
        }
        fn start_microphone(
            &self,
            sink: Arc<dyn MicrophoneFrameSink>,
            cancel: MicrophoneCancel,
        ) -> Result<Box<dyn arcen_deck_core::microphone::MicrophoneStream>, MicrophoneCaptureError>
        {
            arcen_deck_windows::audio::microphone::start_from(
                arcen_deck_windows::audio::microphone::CaptureSource::RenderLoopback,
                sink,
                &cancel,
            )
            .map(|stream| {
                Box::new(stream) as Box<dyn arcen_deck_core::microphone::MicrophoneStream>
            })
        }
        fn probe_network(
            &self,
            host: &str,
            port: u16,
        ) -> arcen_deck_core::network::NetworkProbeResult {
            self.0.probe_network(host, port)
        }
        fn outgoing_interface(
            &self,
            peer: std::net::IpAddr,
        ) -> arcen_deck_core::network::OutgoingInterface {
            self.0.outgoing_interface(peer)
        }
        fn timezone(&self) -> Option<String> {
            self.0.timezone()
        }
    }

    fn session(args: &[String]) -> i32 {
        use arcen_deck_core::runtime::clipboard_controller::ClipboardController;
        use arcen_deck_core::runtime::session::{
            AuthSubmission, ConnectOptions, SessionCommand, SessionEvent, StreamProfile,
            spawn_session,
        };
        use arcen_deck_core::runtime::tls::{TlsTrustConfig, parse_fingerprint};

        let (Some(host), Some(port), Some(pin), Some(seconds)) = (
            args.first().cloned(),
            args.get(1).and_then(|port| port.parse::<u16>().ok()),
            args.get(2).and_then(|pin| parse_fingerprint(pin).ok()),
            args.get(3).and_then(|seconds| seconds.parse::<u64>().ok()),
        ) else {
            eprintln!(
                "usage: platform_lab session HOST PORT CERT_SHA256 SECONDS [--microphone] [--clip-text TEXT]"
            );
            return 2;
        };
        let microphone_loopback = args.iter().any(|arg| arg == "--microphone-loopback");
        let microphone = microphone_loopback || args.iter().any(|arg| arg == "--microphone");
        let clip_text = flag_value(args, "--clip-text");
        let mut lines = std::io::stdin().lines();
        let (Some(Ok(username)), Some(Ok(password))) = (lines.next(), lines.next()) else {
            eprintln!("session: user name and password expected on stdin");
            return 2;
        };

        let services: Arc<WindowsDeckServices> = Arc::new(WindowsDeckServices::new());
        let dyn_services: Arc<dyn DeckServices> = if microphone_loopback {
            Arc::new(LoopbackMicrophone(Arc::clone(&services)))
        } else {
            services.clone()
        };
        // A machine whose only microphone is a silent virtual device hears
        // this tone through the loopback source instead.
        let tone_stop = Arc::new(AtomicBool::new(false));
        let tone = microphone_loopback.then(|| play_tone_until(Arc::clone(&tone_stop)));
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                println!("runtime=FAIL {error}");
                return 1;
            }
        };
        let _entered = runtime.enter();
        let loopback = start_loopback();
        if let Some(loopback) = &loopback {
            println!("loopback_format={}", loopback.format);
            std::thread::sleep(Duration::from_millis(500));
            let _ = loopback_report(loopback, "baseline");
        }
        let monitors = services.displays();
        let options = ConnectOptions {
            display_refresh: None,
            host,
            port,
            use_tls: true,
            username: username.clone(),
            password: password.clone(),
            timeout: Duration::from_secs(30),
            tls: TlsTrustConfig::pinned_certificate(pin),
            profile: StreamProfile::default(),
            monitors,
            displays_mode: "single_primary".to_owned(),
            multi_monitor_topology: None,
            timezone: services.timezone(),
            cursor_preference: arcen_protocol::messages::CursorMode::Local,
            replace_incompatible_desktop: false,
            clipboard_enabled: true,
            microphone_enabled: microphone,
            tablet_input_enabled: false,
            tablet_mode_requested: arcen_protocol::messages::TabletModeMsg::DisabledMouseCompat,
            telemetry: Arc::new(arcen_deck_core::client_telemetry::ClientTelemetry::default()),
            exact_still_image: false,
            canvas: arcen_deck_core::canvas::CanvasRequest::Unavailable,
            services: dyn_services,
        };
        let mut auth = Some(AuthSubmission { username, password });
        let mut handle = spawn_session(options);
        let stop = Arc::new(AtomicBool::new(false));
        let counts = Arc::new((AtomicU64::new(0), AtomicU64::new(0), AtomicI32::new(0)));
        let last_feed: FeedSnapshot = Arc::new(Mutex::new(None));
        let player = spawn_audio_player(
            handle.media.audio_lane().clone(),
            Arc::clone(&stop),
            Arc::clone(&counts),
            Arc::clone(&last_feed),
        );

        let started = Instant::now();
        let deadline = started + Duration::from_secs(seconds);
        let mut controller = ClipboardController::new();
        let mut watcher = WindowsClipboard::open();
        let mut seen_token = watcher.as_mut().map_or(0, PlatformClipboard::change_token);
        let mut clip_sent = false;
        // A legacy sign-in has no `Authenticated` event: the hello that
        // follows a successful sign-in starts the session's work.
        let mut session_at: Option<Instant> = None;
        let mut full_frames = arcen_deck_core::runtime::session::FullFrameRequestGate::default();
        let mut last_ping = Instant::now();
        let mut ping_sequence = 0_u64;
        let mut video_packets = 0_u64;
        let mut last_report = Instant::now();
        let mut ended = None;
        let mut closing = false;
        let mut heard_peak = 0_i32;
        let mut heard_rms = 0.0_f64;
        while ended.is_none() {
            pump_messages();
            while let Ok(event) = handle.events.try_recv() {
                match event {
                    SessionEvent::CertificateUntrusted(_) | SessionEvent::CertificateChanged(_) => {
                        println!("session=FAIL certificate does not match the pin");
                        let _ = handle.commands.send(SessionCommand::Close);
                    }
                    SessionEvent::AuthRequired(_) => {
                        if let Some(submission) = auth.take() {
                            let _ = handle.commands.send(SessionCommand::SubmitAuth(submission));
                        }
                    }
                    SessionEvent::Authenticated(_) => {
                        println!("authenticated elapsed_ms={}", started.elapsed().as_millis());
                        let _ = handle.commands.send(SessionCommand::AcceptAuthentication);
                    }
                    SessionEvent::ServerHello(hello) => {
                        println!(
                            "server_hello supports_audio={} codec={:?} elapsed_ms={}",
                            hello.supports_audio,
                            hello.codec,
                            started.elapsed().as_millis()
                        );
                        controller.attach(1, handle.clipboard.clone());
                        session_at.get_or_insert_with(Instant::now);
                        full_frames.request();
                        let _ = full_frames.send_due(&handle.commands);
                    }
                    SessionEvent::MicrophoneActive(active) => println!(
                        "microphone_active={active} elapsed_ms={}",
                        started.elapsed().as_millis()
                    ),
                    SessionEvent::MediaReady => {
                        let batch = handle.media.take_batch();
                        video_packets = batch.telemetry.video_received;
                        if batch.video.iter().any(|(header, _)| header.is_keyframe()) {
                            full_frames.cancel_pending();
                        }
                        if batch.idr_needed {
                            full_frames.request();
                            let _ = full_frames.send_due(&handle.commands);
                        }
                    }
                    SessionEvent::Ended(end) => ended = Some(end),
                    _ => {}
                }
            }
            if full_frames
                .retry_after()
                .is_some_and(|delay| delay.is_zero())
            {
                let _ = full_frames.send_due(&handle.commands);
            }
            if session_at.is_some() && last_ping.elapsed() >= Duration::from_secs(5) {
                ping_sequence += 1;
                let _ = handle.commands.send(SessionCommand::Json(serde_json::json!({
                    "type": "health_ping",
                    "timestamp_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "sequence": ping_sequence
                })));
                last_ping = Instant::now();
            }
            controller.sync(Instant::now(), &*services);
            if let (Some(at), Some(text), false) = (session_at, clip_text.as_ref(), clip_sent)
                && at.elapsed() >= Duration::from_secs(3)
            {
                let written = foreign_write(13, encode_clipboard_utf16(text));
                println!(
                    "clipboard_local_copy chars={} written={written} elapsed_ms={}",
                    text.chars().count(),
                    started.elapsed().as_millis()
                );
                clip_sent = true;
            }
            if let Some(clipboard) = watcher.as_mut() {
                let token = clipboard.change_token();
                if token != seen_token {
                    seen_token = token;
                    let marked = clipboard.read_origin_marker().is_some();
                    let text = clipboard.read_text();
                    println!(
                        "clipboard_changed from_host={marked} text={:?} elapsed_ms={} unix_s={:.3}",
                        text.as_deref()
                            .map(|text| text.chars().take(80).collect::<String>()),
                        started.elapsed().as_millis(),
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0.0, |since| since.as_secs_f64())
                    );
                }
            }
            if last_report.elapsed() >= Duration::from_secs(5) {
                last_report = Instant::now();
                let feed = last_feed.lock().expect("feed status").clone();
                println!(
                    "progress elapsed_s={} video_packets={video_packets} audio_frames={} audio_played={} decoded_peak={} player={:?} lane={:?}",
                    started.elapsed().as_secs(),
                    counts.0.load(Ordering::Relaxed),
                    counts.1.load(Ordering::Relaxed),
                    counts.2.load(Ordering::Relaxed),
                    feed.map(|status| (
                        status.backend,
                        status.queued_ms,
                        status.queue_phase,
                        status.playback_underruns,
                        status.concealed_frames,
                        status.decode_failures,
                        status.note,
                    )),
                    handle.media.audio_lane().counters(),
                );
                if let Some(loopback) = &loopback {
                    let (peak, rms) = loopback_report(loopback, "session");
                    heard_peak = heard_peak.max(peak);
                    heard_rms = heard_rms.max(rms);
                }
            }
            if !closing && Instant::now() >= deadline {
                closing = true;
                let _ = handle.commands.send(SessionCommand::Close);
            }
            if closing && Instant::now() >= deadline + Duration::from_secs(10) {
                println!("session=FAIL did not end within 10 s of Close");
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::Release);
        let _ = player.join();
        tone_stop.store(true, Ordering::Release);
        if let Some(tone) = tone {
            let _ = tone.join();
        }
        controller.detach();
        if let Some(loopback) = &loopback {
            let (peak, rms) = loopback_report(loopback, "final");
            heard_peak = heard_peak.max(peak);
            heard_rms = heard_rms.max(rms);
        }
        let frames_played = counts.1.load(Ordering::Relaxed);
        println!(
            "session_end={:?} audio_frames={} audio_played={frames_played} decoded_peak={} loopback_heard_peak={heard_peak}",
            ended.as_ref().map(|end| (&end.reason, &end.message)),
            counts.0.load(Ordering::Relaxed),
            counts.2.load(Ordering::Relaxed),
        );
        // The `tone` thresholds: sound, not loopback noise, and the decoded
        // stream itself not silent.
        let decoded_peak = counts.2.load(Ordering::Relaxed);
        let heard = frames_played > 0 && decoded_peak > 0 && heard_peak > 1_000 && heard_rms > 0.02;
        println!("host_audio_heard={}", if heard { "ok" } else { "no" });
        let ended_cleanly = ended.as_ref().is_some_and(|end| {
            matches!(
                end.reason,
                arcen_deck_core::runtime::session::DisconnectReason::Terminal(
                    arcen_deck_core::runtime::session::TerminalDisconnect::Manual
                )
            )
        });
        println!("session_ended_cleanly={ended_cleanly}");
        let expect_audio = args.iter().any(|arg| arg == "--expect-audio");
        i32::from(session_at.is_none() || !ended_cleanly || (expect_audio && !heard))
    }
}

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(lab::run(&args));
}

#[cfg(not(windows))]
fn main() {
    eprintln!("platform_lab runs on Windows only");
}
