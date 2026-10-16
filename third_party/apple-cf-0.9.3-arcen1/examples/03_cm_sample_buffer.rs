//! Smoke test for `apple_cf::cm` — encode an H.264 frame with videotoolbox
//! and inspect the resulting `CMSampleBuffer`'s metadata via the safe
//! `CMSampleBuffer` wrapper.
//!
//! Verifies that the cm/ FFI declarations actually link against the Swift
//! bridge and that retain/release accounting works end-to-end.
//!
//! Run with: `cargo run --example 03_cm_sample_buffer --features cm`
//! (cm is in the default feature set, so just `cargo run --example 03_cm_sample_buffer`)

use apple_cf::cm::CMSampleBuffer;
use apple_cf_compat_04::iosurface::{IOSurface, IOSurfaceLockOptions};
use videotoolbox::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let surface =
        IOSurface::create(160, 120, u32::from_be_bytes(*b"BGRA"), 4).ok_or("alloc IOSurface")?;
    {
        let mut g = surface
            .lock(IOSurfaceLockOptions::NONE)
            .map_err(|c| format!("lock: {c}"))?;
        if let Some(b) = g.as_slice_mut() {
            for px in b.chunks_exact_mut(4) {
                px[0] = 0x40;
                px[1] = 0x80;
                px[2] = 0xC0;
                px[3] = 0xFF;
            }
        }
    }

    let encoder = CompressionSession::builder(160, 120, Codec::H264)
        .with_real_time(true)
        .with_expected_frame_rate(30.0)
        .build()?;
    let frame = encoder.encode(&surface, (0, 30))?;

    println!("Encoded {} bytes of H.264", frame.data.len());

    // videotoolbox 0.10 still depends on apple-cf 0.4.x, so this example
    // allocates its IOSurface through the renamed compatibility dependency
    // above. The encoder hands us back a raw CMSampleBufferRef (opaque);
    // wrap that pointer in the current apple_cf::cm::CMSampleBuffer API.
    // The wrapper retains-on-take so videotoolbox's EncodedFrame still owns
    // the original.
    let raw_ptr = frame.cm_sample_buffer_ptr();
    let sample_buffer = unsafe { CMSampleBuffer::from_raw_retained(raw_ptr) }
        .ok_or("CMSampleBuffer wrap returned None")?;

    println!("CMSampleBuffer:");
    println!("  is_valid:      {}", sample_buffer.is_valid());
    println!("  data_is_ready: {}", sample_buffer.data_is_ready());
    println!("  num_samples:   {}", sample_buffer.num_samples());
    let pts = sample_buffer.presentation_timestamp();
    println!(
        "  pts:           {} / {} (flags=0x{:x})",
        pts.value, pts.timescale, pts.flags
    );
    let dur = sample_buffer.duration();
    println!("  duration:      {} / {}", dur.value, dur.timescale);

    let block_buffer = sample_buffer
        .data_buffer()
        .ok_or("expected a data buffer for compressed video")?;
    println!("  block_buffer.len: {}", block_buffer.data_length());

    let format = sample_buffer
        .format_description()
        .ok_or("expected a format description")?;
    println!(
        "  format: media_type={} subtype={}",
        format.media_type().display(),
        format.media_subtype().display()
    );

    // The format description's media subtype should be 'avc1' for H.264.
    assert_eq!(
        format.media_subtype().as_u32(),
        u32::from_be_bytes(*b"avc1"),
        "expected H.264 ('avc1') subtype"
    );

    Ok(())
}
