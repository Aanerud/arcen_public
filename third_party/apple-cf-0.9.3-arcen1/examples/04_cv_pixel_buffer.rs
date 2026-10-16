//! Smoke test for `apple_cf::cv` — wrap an `IOSurface` in a `CVPixelBuffer`
//! and verify the round-trip plus pixel-buffer accessors.
//!
//! Run with: `cargo run --example 04_cv_pixel_buffer`

use apple_cf::cv::CVPixelBuffer;
use apple_cf::iosurface::{IOSurface, IOSurfaceLockOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pixel_format = u32::from_be_bytes(*b"BGRA");
    let width = 32usize;
    let height = 16usize;

    // 1. Allocate an IOSurface, write a sentinel pixel.
    let surface = IOSurface::create(width, height, pixel_format, 4).ok_or("alloc IOSurface")?;
    {
        let mut g = surface
            .lock(IOSurfaceLockOptions::NONE)
            .map_err(|c| format!("lock: {c}"))?;
        if let Some(b) = g.as_slice_mut() {
            b[0] = 0xDE;
            b[1] = 0xAD;
            b[2] = 0xBE;
            b[3] = 0xEF;
        }
    }
    println!(
        "IOSurface: {}x{} BGRA, id={}",
        surface.width(),
        surface.height(),
        surface.id()
    );

    // 2. Wrap in a CVPixelBuffer (zero-copy — the CVPixelBuffer references
    //    the same backing IOSurface).
    let pb = CVPixelBuffer::create_with_io_surface(&surface)
        .map_err(|c| format!("CVPixelBuffer::create_with_io_surface: {c}"))?;
    println!(
        "CVPixelBuffer: {}x{} format=0x{:08x} bytes_per_row={} planar={}",
        pb.width(),
        pb.height(),
        pb.pixel_format(),
        pb.bytes_per_row(),
        pb.is_planar(),
    );
    assert_eq!(pb.width(), width);
    assert_eq!(pb.height(), height);
    assert_eq!(pb.pixel_format(), pixel_format);
    assert!(!pb.is_planar());

    // 3. Lock the pixel buffer for read and verify the sentinel pixel
    //    written via the IOSurface is visible.
    let guard = pb.lock_read_only().map_err(|c| format!("CV lock: {c}"))?;
    let bytes = guard.as_slice();
    assert!(!bytes.is_empty(), "locked pixel buffer should expose data");
    println!(
        "  pixel[0..4] = [{:02x}, {:02x}, {:02x}, {:02x}]",
        bytes[0], bytes[1], bytes[2], bytes[3]
    );
    assert_eq!(&bytes[0..4], &[0xDE, 0xAD, 0xBE, 0xEF]);
    drop(guard);

    // 4. Round-trip back: CVPixelBuffer should report the same IOSurface.
    let surface_from_pb = pb.io_surface().ok_or("pb.io_surface")?;
    println!(
        "  io_surface roundtrip: id={} (expected {})",
        surface_from_pb.id(),
        surface.id()
    );
    assert_eq!(surface_from_pb.id(), surface.id());

    println!("\nOK CVPixelBuffer round-trip works end-to-end");
    Ok(())
}
