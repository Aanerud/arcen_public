#![allow(unsafe_code, clippy::expect_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::sync::Mutex;

use arcen_media::audio::{
    AudioBitrateTier, MICROPHONE_V1_FRAME_SAMPLES, MicrophoneFrameReceiver,
    ResolvedMicrophoneStream,
};
#[cfg(feature = "audio-opus")]
use arcen_media::audio::{AudioFrameSpec, MAX_OPUS_PACKET_BYTES, MicrophoneDecoder, OpusEncoder};
use arcen_protocol::messages::MicrophoneStreamReason;
use arcen_protocol::{AudioCodec, MICROPHONE_PCM_BYTES, MicrophoneHeader};

struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_allocation();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn record_allocation() {
    COUNTING.with(|counting| {
        if counting.get() {
            ALLOCATIONS.with(|allocations| allocations.set(allocations.get() + 1));
        }
    });
}

fn count_allocations(run: impl FnOnce()) -> usize {
    let _guard = TEST_LOCK.lock().expect("allocation test lock");
    ALLOCATIONS.with(|allocations| allocations.set(0));
    COUNTING.with(|counting| counting.set(true));
    run();
    COUNTING.with(|counting| counting.set(false));
    ALLOCATIONS.with(Cell::get)
}

fn stream(codec: AudioCodec, bitrate: AudioBitrateTier) -> ResolvedMicrophoneStream {
    ResolvedMicrophoneStream {
        codec: Some(codec),
        bitrate,
        generation: 7,
        reason: MicrophoneStreamReason::Enabled,
    }
}

fn header(codec: AudioCodec, sequence: u32) -> MicrophoneHeader {
    MicrophoneHeader {
        codec,
        sequence,
        timestamp_ms: (sequence - 1) * 20,
        generation: 7,
    }
}

#[test]
fn repeated_pcm_ingest_and_pop_have_no_transient_allocations() {
    let mut receiver = MicrophoneFrameReceiver::new(stream(AudioCodec::Pcm, AudioBitrateTier::Off));
    let payload = [1u8; MICROPHONE_PCM_BYTES];
    let mut output = [0i16; MICROPHONE_V1_FRAME_SAMPLES];
    for sequence in 1..=3 {
        receiver
            .ingest_pcm(header(AudioCodec::Pcm, sequence), &payload)
            .expect("PCM warmup ingest");
    }
    receiver.pop_into(&mut output).expect("PCM warmup pop");

    let allocations = count_allocations(|| {
        for sequence in 4..=131 {
            black_box(
                receiver
                    .ingest_pcm(header(AudioCodec::Pcm, sequence), &payload)
                    .expect("PCM ingest"),
            );
            black_box(receiver.pop_into(&mut output).expect("PCM pop"));
        }
    });

    assert_eq!(allocations, 0);
}

#[cfg(feature = "audio-opus")]
#[test]
fn repeated_opus_ingest_and_pop_have_no_transient_allocations() {
    let stream = stream(AudioCodec::Opus, AudioBitrateTier::Kbps64);
    let mut encoder =
        OpusEncoder::new_for_spec(AudioFrameSpec::MICROPHONE_V1, stream.bitrate).expect("encoder");
    let mut decoder = MicrophoneDecoder::new(stream).expect("decoder");
    let input = [0i16; MICROPHONE_V1_FRAME_SAMPLES];
    let mut packet = [0u8; MAX_OPUS_PACKET_BYTES];
    let packet_len = encoder.encode(&input, &mut packet).expect("encode");
    let mut output = [0i16; MICROPHONE_V1_FRAME_SAMPLES];
    for sequence in 1..=3 {
        decoder
            .ingest(header(AudioCodec::Opus, sequence), &packet[..packet_len])
            .expect("Opus warmup ingest");
    }
    decoder.pop_into(&mut output).expect("Opus warmup pop");

    let allocations = count_allocations(|| {
        for sequence in 4..=131 {
            black_box(
                decoder
                    .ingest(header(AudioCodec::Opus, sequence), &packet[..packet_len])
                    .expect("Opus ingest"),
            );
            black_box(decoder.pop_into(&mut output).expect("Opus pop"));
        }
    });

    assert_eq!(allocations, 0);
}
