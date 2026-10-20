//! Bringing a `ScreenCast.RecordVirtual` monitor into existence at an exact
//! size and refresh.
//!
//! Mutter creates a virtual stream's monitor at the size and maximum frame
//! rate its consumer negotiates, keeps it when the consumer leaves, and
//! resizes it in place when a later consumer negotiates another size. The
//! Linux Pier's session agent uses this to create and resize a remote-monitor
//! session's outputs before any capture epoch connects: it negotiates the
//! exact geometry, waits for the producer to fix it and deliver one buffer,
//! and disconnects. No pixel is read.

use std::time::{Duration, Instant};

use super::format::{FormatOffer, MemoryOffer};
use super::handoff::WaylandHandoff;
use super::pipewire::{PipeWireSource, SourceConfig};
use arcen_media::video::CapturePixelFormat;

/// What a negotiation produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaterialisedOutput {
    /// Negotiated width.
    pub width: u32,
    /// Negotiated height.
    pub height: u32,
    /// Negotiated maximum frame rate as `num/den`.
    pub max_framerate: (u32, u32),
    /// Whether a buffer arrived before the consumer left.
    pub buffer_seen: bool,
}

/// Negotiates `width`×`height` at up to `max_fps` on the hand-off's node,
/// waits up to `timeout` for the format and one buffer, and disconnects.
///
/// # Errors
///
/// A description when `PipeWire` cannot be reached, the producer fixes no
/// format in time, or it fixes another size.
pub fn materialise_virtual_output(
    handoff: &WaylandHandoff,
    width: u32,
    height: u32,
    max_fps: u32,
    timeout: Duration,
) -> Result<MaterialisedOutput, String> {
    let deadline = Instant::now() + timeout;
    let source = PipeWireSource::start(
        handoff,
        SourceConfig {
            offer: FormatOffer {
                formats: vec![CapturePixelFormat::Bgrx8, CapturePixelFormat::Bgra8],
                size: Some((width, height)),
                max_fps: max_fps.max(1),
                memory: MemoryOffer::HostOnly,
            },
            mailbox_capacity: 2,
            source_gpu: None,
            encoder_gpu: None,
            dmabuf_copy_path: None,
            explicit_sync: None,
        },
    )
    .map_err(|error| error.to_string())?;
    let negotiated = source
        .wait_negotiated(deadline.saturating_duration_since(Instant::now()))
        .ok_or_else(|| format!("no format was fixed within {timeout:?}"))?;
    if (negotiated.width, negotiated.height) != (width, height) {
        return Err(format!(
            "the producer fixed {}x{}, {width}x{height} was requested",
            negotiated.width, negotiated.height
        ));
    }
    let buffer_seen = matches!(
        source.next_frame(deadline.saturating_duration_since(Instant::now())),
        Ok(Some(_))
    );
    drop(source);
    Ok(MaterialisedOutput {
        width,
        height,
        max_framerate: negotiated.max_framerate,
        buffer_seen,
    })
}
