//! Smoke test for `apple_cf::cg::CGContext` bitmap drawing.
//!
//! Run with: `cargo run --example 05_cgcontext_smoke --features cg`

use std::error::Error;
use std::path::PathBuf;

use apple_cf::cg::CGContext;

fn pixel_rgba(bytes: &[u8], bytes_per_row: usize, x: usize, y: usize) -> [u8; 4] {
    let offset = y * bytes_per_row + x * 4;
    [
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]
}

fn main() -> Result<(), Box<dyn Error>> {
    let context = CGContext::new_rgba8(64, 64)?;

    assert_eq!(context.width(), 64);
    assert_eq!(context.height(), 64);
    assert_eq!(context.bits_per_component(), 8);
    assert_eq!(context.bits_per_pixel(), 32);
    assert!(context.bytes_per_row() >= 64 * 4);
    assert!(!context.data().is_null());
    assert_eq!(context.alpha_info(), 1);

    context.set_rgb_fill_color(1.0, 1.0, 1.0, 1.0);
    context.fill_rect(0.0, 0.0, 64.0, 64.0);

    context.save_g_state();
    context.translate(0.0, 64.0);
    context.scale(1.0, -1.0);

    context.set_rgb_fill_color(1.0, 0.0, 0.0, 1.0);
    context.fill_rect(0.0, 0.0, 16.0, 16.0);

    context.set_rgb_stroke_color(0.0, 0.0, 1.0, 1.0);
    context.set_line_width(2.0);
    context.begin_path();
    context.add_ellipse_in_rect(20.0, 20.0, 24.0, 24.0);
    context.stroke_path();

    context.restore_g_state();

    let image = context
        .snapshot_to_image()
        .ok_or("snapshot_to_image returned None")?;
    let png_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/cgcontext_smoke.png");
    image.save_png(&png_path)?;

    let bytes = context.as_bytes();
    let bottom_left = pixel_rgba(bytes, context.bytes_per_row(), 0, 0);
    let top_right = pixel_rgba(bytes, context.bytes_per_row(), 63, 63);
    assert_eq!(bottom_left, [255, 0, 0, 255]);
    assert_eq!(top_right, [255, 255, 255, 255]);

    println!("saved snapshot: {}", png_path.display());
    println!("✅ context drew successfully");
    Ok(())
}
