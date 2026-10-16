use apple_cf::cf::CFString;
use apple_cf::cv::{CVAttachmentMode, CVBuffer, CVImageBuffer, CVMetalTextureCache, CVPixelBuffer};

#[test]
fn cv_buffer_attachment_round_trip() {
    let pixel_buffer = CVPixelBuffer::create(16, 16, 0x4247_5241).expect("pixel buffer");
    let buffer = CVBuffer::from_pixel_buffer(&pixel_buffer).expect("buffer");
    let key = CFString::new("com.doomfish.apple-cf.cv-buffer-tests");
    let value = CFString::new("attached");

    buffer.set_attachment(&key, &value, CVAttachmentMode::ShouldPropagate);
    assert!(buffer.attachment(&key).is_some());
    assert!(buffer
        .attachments(CVAttachmentMode::ShouldPropagate)
        .is_some());
    buffer.remove_all_attachments();
    assert!(buffer.attachment(&key).is_none());
}

#[test]
fn cv_image_buffer_smoke() {
    let pixel_buffer = CVPixelBuffer::create(8, 4, 0x4247_5241).expect("pixel buffer");
    let image_buffer = CVImageBuffer::from_pixel_buffer(&pixel_buffer).expect("image buffer");
    assert!((image_buffer.encoded_size().width - 8.0).abs() < f64::EPSILON);
    assert!((image_buffer.display_size().height - 4.0).abs() < f64::EPSILON);
}

#[test]
fn cv_metal_texture_cache_smoke() {
    let cache = CVMetalTextureCache::system_default().expect("system default Metal device");
    cache.flush();
}
