//! What capenc offers a `PipeWire` producer and what was negotiated.
//!
//! Pure data. The `PipeWire` adapter turns a [`FormatOffer`] into SPA pods and
//! a negotiated SPA format back into a [`NegotiatedFormat`]; everything a
//! pipeline needs to decide anything is expressed here first.

use arcen_media::video::{CapturePixelFormat, FormatModifier, SourceColour};

/// Largest cursor bitmap capenc accepts, per side. GNOME 40 Mutter offers
/// cursor metadata only for a 384x384 bitmap; a consumer whose range stops
/// below that receives no cursor metadata at all.
pub const CURSOR_BITMAP_MAX_SIDE: u32 = 384;
/// `sizeof(struct spa_meta_cursor)`.
pub const SPA_META_CURSOR_BYTES: usize = 28;
/// `sizeof(struct spa_meta_bitmap)`.
pub const SPA_META_BITMAP_BYTES: usize = 20;
/// `sizeof(struct spa_meta_header)`.
pub const SPA_META_HEADER_BYTES: usize = 32;
/// `sizeof(struct spa_meta_region)`.
pub const SPA_META_REGION_BYTES: usize = 16;
/// Damage regions capenc asks for; more is treated as full damage.
pub const DAMAGE_META_MAX_REGIONS: usize = 32;

/// `CURSOR_META_SIZE(w, h)`: cursor header, bitmap header and ARGB pixels.
#[must_use]
pub const fn cursor_meta_size(width: u32, height: u32) -> usize {
    SPA_META_CURSOR_BYTES + SPA_META_BITMAP_BYTES + (width as usize) * (height as usize) * 4
}

/// Buffer memory a consumer accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryOffer {
    /// Shared memory only (`MemFd`, `MemPtr`).
    HostOnly,
    /// DMA-BUF with exactly these modifiers, then shared memory as a separate,
    /// lower-preference format. Only an importer that listed a modifier may
    /// receive it.
    DmaBufThenHost {
        /// Modifiers the GPU importer reported for the offered formats.
        modifiers: Vec<FormatModifier>,
    },
}

impl MemoryOffer {
    /// Whether DMA-BUF is offered at all.
    #[must_use]
    pub const fn offers_dmabuf(&self) -> bool {
        matches!(self, Self::DmaBufThenHost { .. })
    }
}

/// The formats, size and cadence a capture epoch asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatOffer {
    /// Acceptable formats, most preferred first. Each pipeline chooses its
    /// own list; the eight-bit SDR pipelines offer the BGRx/BGRA family.
    pub formats: Vec<CapturePixelFormat>,
    /// Requested size, or `None` to accept the producer's size.
    pub size: Option<(u32, u32)>,
    /// Highest frame rate the pipeline can use.
    pub max_fps: u32,
    /// Memory kinds.
    pub memory: MemoryOffer,
}

impl FormatOffer {
    /// The eight-bit SDR BGRx/BGRA family over shared memory: what GNOME 40
    /// provides, and what the host-copy importer stages without a swizzle.
    #[must_use]
    pub fn sdr8_host(max_fps: u32) -> Self {
        Self {
            formats: vec![CapturePixelFormat::Bgrx8, CapturePixelFormat::Bgra8],
            size: None,
            max_fps,
            memory: MemoryOffer::HostOnly,
        }
    }

    /// Wide formats in `preference` order over shared memory; the wide
    /// runner replaces the memory with DMA-BUF when its read-back opened.
    /// Never mixed with the eight-bit family: a wide composition that
    /// negotiates eight bits has nothing genuine to serve.
    #[must_use]
    pub fn wide_host(preference: &[CapturePixelFormat], max_fps: u32) -> Self {
        Self {
            formats: preference
                .iter()
                .copied()
                .filter(|format| format.precision().is_wide())
                .collect(),
            size: None,
            max_fps,
            memory: MemoryOffer::HostOnly,
        }
    }
}

/// The format a producer fixed for the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedFormat {
    /// Pixel format.
    pub format: CapturePixelFormat,
    /// Coded width.
    pub width: u32,
    /// Coded height.
    pub height: u32,
    /// DRM modifier, when the producer negotiated one (DMA-BUF only).
    pub modifier: Option<FormatModifier>,
    /// Colour facts the producer attached to the format.
    pub colour: SourceColour,
    /// Producer's nominal frame rate as `num/den`; `0/1` means variable.
    pub framerate: (u32, u32),
    /// Producer's maximum frame rate as `num/den`.
    pub max_framerate: (u32, u32),
}

impl NegotiatedFormat {
    /// Whether this format is one `offer` asked for.
    #[must_use]
    pub fn satisfies(&self, offer: &FormatOffer) -> bool {
        offer.formats.contains(&self.format)
            && self.width > 0
            && self.height > 0
            && offer
                .size
                .is_none_or(|(width, height)| (width, height) == (self.width, self.height))
            && (self.modifier.is_none() || offer.memory.offers_dmabuf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_meta_size_matches_the_mutter_40_buffer() {
        // Observed on GNOME 40: `Meta {"type": "Cursor", "size": 589872}`.
        assert_eq!(
            cursor_meta_size(CURSOR_BITMAP_MAX_SIDE, CURSOR_BITMAP_MAX_SIDE),
            589_872
        );
        assert_eq!(cursor_meta_size(0, 0), 48);
    }

    #[test]
    fn negotiated_format_must_match_the_offer() {
        let offer = FormatOffer::sdr8_host(30);
        let negotiated = NegotiatedFormat {
            format: CapturePixelFormat::Bgrx8,
            width: 1920,
            height: 1080,
            modifier: None,
            colour: SourceColour::UNKNOWN,
            framerate: (0, 1),
            max_framerate: (60, 1),
        };
        assert!(negotiated.satisfies(&offer));
        let wide = NegotiatedFormat {
            format: CapturePixelFormat::Xrgb2101010,
            ..negotiated
        };
        assert!(!wide.satisfies(&offer));
        let dmabuf = NegotiatedFormat {
            modifier: Some(FormatModifier::LINEAR),
            ..negotiated
        };
        assert!(!dmabuf.satisfies(&offer));
        let sized = FormatOffer {
            size: Some((1280, 720)),
            ..offer.clone()
        };
        assert!(!negotiated.satisfies(&sized));
        let empty = NegotiatedFormat {
            width: 0,
            ..negotiated
        };
        assert!(!empty.satisfies(&offer));
    }
}
