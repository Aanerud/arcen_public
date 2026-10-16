//! Smoke test: create a tiny `IOSurface`, write a pixel, read it back.
//!
//! Run with: `cargo run --example 01_iosurface_smoke`

use apple_cf::iosurface::{IOSurface, IOSurfaceLockOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // BGRA = 'BGRA' = 0x42475241
    let pixel_format: u32 = u32::from_be_bytes(*b"BGRA");
    let width = 16;
    let height = 16;
    let bytes_per_element = 4;

    let surface = IOSurface::create(width, height, pixel_format, bytes_per_element)
        .ok_or("failed to create IOSurface")?;

    println!(
        "Created IOSurface: {}x{}",
        surface.width(),
        surface.height()
    );
    println!("  pixel_format = 0x{:08x}", surface.pixel_format());
    println!("  bytes_per_row = {}", surface.bytes_per_row());
    println!("  alloc_size = {}", surface.alloc_size());

    {
        let mut guard = surface
            .lock(IOSurfaceLockOptions::NONE)
            .map_err(|c| format!("lock failed: {c}"))?;
        if let Some(bytes) = guard.as_slice_mut() {
            bytes[0] = 0xFF;
            bytes[1] = 0x00;
            bytes[2] = 0x80;
            bytes[3] = 0xFF;
        }
    }

    let guard = surface
        .lock(IOSurfaceLockOptions::READ_ONLY)
        .map_err(|c| format!("read lock failed: {c}"))?;
    let bytes = guard.as_slice();
    println!(
        "  pixel[0] = B={:02x} G={:02x} R={:02x} A={:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    );
    Ok(())
}
