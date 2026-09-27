//! Writes one shared colour test pattern as raw BGRA to standard output.
//!
//! `cargo run -p arcen-media --example render_test_pattern -- chroma_detail 1800 1130 > pattern.bgra`
//!
//! A host shows these bytes full-screen at one pixel per pixel, and a client
//! measures its decoded frame against the same pattern, so both ends use the
//! one definition in `arcen_media::test_pattern`.

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: render_test_pattern <token> <width> <height>";
    let pattern = args
        .get(1)
        .and_then(|token| arcen_media::test_pattern::TestPattern::from_token(token))
        .expect(usage);
    let width: usize = args
        .get(2)
        .and_then(|value| value.parse().ok())
        .expect(usage);
    let height: usize = args
        .get(3)
        .and_then(|value| value.parse().ok())
        .expect(usage);
    std::io::stdout()
        .lock()
        .write_all(&pattern.render_bgra(width, height))
        .expect("write pattern");
}
