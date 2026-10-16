use apple_cf::cf::CFString;
use apple_cf::cv::{CVAttachmentMode, CVBuffer, CVImageBuffer, CVPixelBuffer};

fn main() {
    let pixel_buffer = CVPixelBuffer::create(16, 16, 0x4247_5241).expect("pixel buffer");
    let buffer = CVBuffer::from_pixel_buffer(&pixel_buffer).expect("buffer");
    let key = CFString::new("com.doomfish.apple-cf.example");
    let value = CFString::new("attached");
    buffer.set_attachment(&key, &value, CVAttachmentMode::ShouldPropagate);
    assert!(buffer.attachment(&key).is_some());

    let image = CVImageBuffer::from_pixel_buffer(&pixel_buffer).expect("image buffer");
    assert!((image.encoded_size().width - 16.0).abs() < f64::EPSILON);
    assert!((image.display_size().height - 16.0).abs() < f64::EPSILON);
}
