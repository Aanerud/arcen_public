// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Host cursor images between a Pier and a Deck.
//!
//! The Pier turns its desktop's cursor into the current cursor — from a
//! Wayland capture process's per-output events ([`CursorAggregate`]) or from
//! a host that reads its one pointer directly, X11 or Windows
//! ([`HostCursor`]) — gives every distinct image a stable id
//! ([`CursorImageRegistry`], one per desktop) and sends the newest state on
//! the input/cursor lane: itself ([`CursorLaneOutbox`], one per attachment)
//! or, from a session agent, through its relay
//! ([`agent_cursor_record`] → `arcen_protocol::cursor::CursorRecordRelay`).
//! The Deck applies those records ([`DeckCursorState`]). Named host shapes
//! ([`shape_for_x_cursor_name`]) are the fallback for a Deck that does not
//! draw images.
//!
//! An image crosses the link once. Both ends keep the same deterministic
//! [`CursorImageCache`]: every delivered record touches or inserts exactly
//! one entry, weighted by the image's size from the record header, in lane
//! order, and both ends clear it when the lane is reopened. The Pier keeps
//! only a mirror of ids, so it knows whether the Deck holds an image without
//! any acknowledgement, and inlines the PNG only when it does not.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use arcen_media::capenc_stream::{CapencCursorEvent, CapencCursorSprite};
use arcen_media::cursor::{CursorImage, CursorImageError, WireCursorImage};
pub use arcen_protocol::cursor::{CURSOR_CACHE_BYTES, CURSOR_CACHE_ENTRIES, CursorImageCache};
use arcen_protocol::cursor::{
    CursorAppearance, CursorImageId, CursorImageRef, CursorUpdate, CursorUpdateError,
    decode_cursor_update, encode_cursor_update,
};
use arcen_protocol::messages::CursorShapeKind;

/// Distinct images a Pier remembers for one desktop.
pub const CURSOR_REGISTRY_ENTRIES: usize = 128;
/// Source and PNG bytes a Pier remembers for one desktop.
pub const CURSOR_REGISTRY_BYTES: usize = 8 * 1024 * 1024;

struct RegisteredImage {
    wire: Arc<WireCursorImage>,
    source: CursorImage,
    last_used: u64,
}

/// Distinct cursor images of one desktop, each with a stable id.
///
/// Ids are never reused, so evicting an image only means that a later
/// appearance of it is encoded again under a new id.
#[derive(Default)]
pub struct CursorImageRegistry {
    by_source: HashMap<CursorImage, CursorImageId>,
    images: HashMap<CursorImageId, RegisteredImage>,
    next_id: u32,
    clock: u64,
    bytes: usize,
}

impl std::fmt::Debug for CursorImageRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CursorImageRegistry")
            .field("images", &self.images.len())
            .field("bytes", &self.bytes)
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl CursorImageRegistry {
    /// The id and wire form of `image`; `Ok(None)` when no pixel of it is
    /// visible, which a desktop uses to hide the pointer.
    ///
    /// # Errors
    ///
    /// [`CursorImageError`] when the image cannot be fitted to the wire.
    pub fn register(
        &mut self,
        image: &CursorImage,
    ) -> Result<Option<(CursorImageId, Arc<WireCursorImage>)>, CursorImageError> {
        self.clock += 1;
        if let Some(id) = self.by_source.get(image).copied() {
            if let Some(entry) = self.images.get_mut(&id) {
                entry.last_used = self.clock;
                return Ok(Some((id, Arc::clone(&entry.wire))));
            }
        }
        let Some(wire) = image.to_wire()? else {
            return Ok(None);
        };
        self.next_id = self.next_id.checked_add(1).unwrap_or(1);
        let Some(id) = CursorImageId::new(self.next_id) else {
            return Ok(None);
        };
        let wire = Arc::new(wire);
        let weight = image.rgba().len() + wire.png.len();
        self.bytes += weight;
        self.by_source.insert(image.clone(), id);
        self.images.insert(
            id,
            RegisteredImage {
                wire: Arc::clone(&wire),
                source: image.clone(),
                last_used: self.clock,
            },
        );
        self.evict(id);
        Ok(Some((id, wire)))
    }

    fn evict(&mut self, keep: CursorImageId) {
        while self.images.len() > 1
            && (self.images.len() > CURSOR_REGISTRY_ENTRIES || self.bytes > CURSOR_REGISTRY_BYTES)
        {
            let Some(oldest) = self
                .images
                .iter()
                .filter(|(id, _)| **id != keep)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(id, _)| *id)
            else {
                return;
            };
            if let Some(entry) = self.images.remove(&oldest) {
                self.bytes -= entry.source.rgba().len() + entry.wire.png.len();
                self.by_source.remove(&entry.source);
            }
        }
    }

    /// Whether `image` already has an id (registering it again is cheap).
    #[must_use]
    pub fn contains(&self, image: &CursorImage) -> bool {
        self.by_source.contains_key(image)
    }

    /// Number of images remembered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// Whether nothing is remembered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }
}

/// The current cursor of a desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorFacts {
    /// The desktop hides the pointer.
    Hidden {
        /// Output that reported it.
        output: u32,
        /// That output's generation.
        generation: u32,
    },
    /// The desktop shows this image.
    Image {
        /// Output whose pixel space the image is in.
        output: u32,
        /// That output's generation.
        generation: u32,
        /// Image id.
        id: CursorImageId,
        /// The image as it travels.
        wire: Arc<WireCursorImage>,
        /// The named shape closest to the image, for a client that cannot
        /// show images (`default` when the host cannot name it).
        fallback: CursorShapeKind,
    },
}

impl CursorFacts {
    /// The named shape a client without cursor images shows.
    #[must_use]
    pub const fn fallback_shape(&self) -> CursorShapeKind {
        match self {
            Self::Hidden { .. } => CursorShapeKind::Hidden,
            Self::Image { fallback, .. } => *fallback,
        }
    }
}

#[derive(Debug, Clone)]
enum OutputSprite {
    Empty,
    Image(CursorImageId, Arc<WireCursorImage>),
}

#[derive(Debug, Clone, Default)]
struct OutputCursor {
    generation: u32,
    sprite: Option<OutputSprite>,
    on_output: bool,
}

/// What [`CursorAggregate::apply`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorAggregateOutcome {
    /// The current cursor changed.
    Changed,
    /// Nothing visible changed.
    Unchanged,
    /// The image could not be fitted to the wire; the output's last sprite
    /// stays.
    ImageRefused(CursorImageError),
}

/// The current cursor of a desktop, from its outputs' capture events.
///
/// Mutter reports a cursor on an output with a nonzero id, and id 0 when
/// the cursor is not on that output or the pointer is not visible (a client
/// that sets no cursor surface, measured on Mutter 49.4); an all-zero
/// bitmap is an empty sprite. So the pointer is hidden when the sprite of
/// the output that has it is empty, or when no output reports it at all;
/// the output that most recently reported it decides otherwise, and a
/// position-only update keeps the last sprite. It lives as long as the
/// desktop, because Mutter sends a sprite only when it changes: a capture
/// process that reconnects to an existing stream gets positions only, and
/// the remembered sprite fills in.
#[derive(Debug, Default)]
pub struct CursorAggregate {
    outputs: HashMap<u32, OutputCursor>,
    active: Option<u32>,
    unreported: bool,
    /// The sprite any output reported last: an output that never had one
    /// (its capture joined a running stream) shows it.
    last_sprite: Option<OutputSprite>,
    registry: CursorImageRegistry,
    current: Option<CursorFacts>,
}

impl CursorAggregate {
    /// Applies one capture event of `output` at `generation`.
    pub fn apply(
        &mut self,
        output: u32,
        generation: u32,
        event: &CapencCursorEvent,
    ) -> CursorAggregateOutcome {
        let mut refused = None;
        let entry = self.outputs.entry(output).or_default();
        if entry.generation != generation {
            entry.generation = generation;
        }
        match &event.sprite {
            CapencCursorSprite::Unchanged => {}
            CapencCursorSprite::Empty => entry.sprite = Some(OutputSprite::Empty),
            CapencCursorSprite::Image(image) => match self.registry.register(image) {
                Ok(Some((id, wire))) => entry.sprite = Some(OutputSprite::Image(id, wire)),
                Ok(None) => entry.sprite = Some(OutputSprite::Empty),
                Err(error) => refused = Some(error),
            },
        }
        if !matches!(event.sprite, CapencCursorSprite::Unchanged) && entry.sprite.is_some() {
            self.last_sprite.clone_from(&entry.sprite);
        }
        entry.on_output = event.on_output;
        if event.on_output {
            self.active = Some(output);
        }
        let reported = self.outputs.values().any(|cursor| cursor.on_output);
        self.unreported = !reported;
        let next = self.active.and_then(|active| {
            let cursor = self.outputs.get(&active)?;
            if !reported {
                // No output shows the cursor: the pointer is hidden.
                return Some(CursorFacts::Hidden {
                    output: active,
                    generation: cursor.generation,
                });
            }
            Some(
                match cursor.sprite.as_ref().or(self.last_sprite.as_ref())? {
                    OutputSprite::Empty => CursorFacts::Hidden {
                        output: active,
                        generation: cursor.generation,
                    },
                    OutputSprite::Image(id, wire) => CursorFacts::Image {
                        output: active,
                        generation: cursor.generation,
                        id: *id,
                        wire: Arc::clone(wire),
                        fallback: CursorShapeKind::Default,
                    },
                },
            )
        });
        if let Some(error) = refused {
            if next.is_some() {
                self.current = next;
            }
            return CursorAggregateOutcome::ImageRefused(error);
        }
        if next.is_some() && next != self.current {
            self.current = next;
            CursorAggregateOutcome::Changed
        } else {
            CursorAggregateOutcome::Unchanged
        }
    }

    /// The current cursor, once any output reported one.
    #[must_use]
    pub const fn current(&self) -> Option<&CursorFacts> {
        self.current.as_ref()
    }

    /// Whether the cursor is hidden because no output reports it while the
    /// desktop has several outputs: when the pointer crosses outputs, the
    /// one it left may report before the one it entered (each output has
    /// its own capture), so this may be a crossing rather than a hidden
    /// pointer until the other outputs had a moment to report.
    #[must_use]
    pub fn hidden_while_crossing(&self) -> bool {
        self.unreported && self.outputs.len() > 1
    }

    /// Forgets an output that no longer exists.
    pub fn remove_output(&mut self, output: u32) {
        self.outputs.remove(&output);
        if self.active == Some(output) {
            self.active = None;
        }
    }

    /// Distinct images remembered for this desktop.
    #[must_use]
    pub fn registered_images(&self) -> usize {
        self.registry.len()
    }
}

/// New cursor images a host fits to the wire in any one second.
///
/// Fitting an image (trim, resample, PNG) is the expensive step and an
/// image travels at most once per lane, so this bounds what an application
/// that churns cursor images (an animated cursor's first cycle, or a
/// misbehaving one) can cost the host and the link. Known images are not
/// counted: an animated cursor that repeats costs ids only.
pub const MAX_NEW_CURSOR_IMAGES_PER_SECOND: usize = 30;

/// What a host reads from its desktop's pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCursorSample {
    /// The pointer is hidden.
    Hidden,
    /// The pointer shows `image`.
    Image {
        /// The image, in the stream's pixels.
        image: CursorImage,
        /// Its closest named shape, for a Deck that cannot show images
        /// (`default` when the host cannot name it).
        fallback: CursorShapeKind,
    },
}

/// What [`HostCursor::offer`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCursorOutcome {
    /// The current cursor changed.
    Changed,
    /// Nothing visible changed.
    Unchanged,
    /// Too many new images this second: the newest sample waits for
    /// [`HostCursor::poll`] at [`HostCursor::retry_at`].
    Deferred,
    /// The image could not be fitted to the wire; the current cursor stays.
    ImageRefused(CursorImageError),
}

/// The cursor of a desktop with one pointer source, read directly from the
/// host (X11 `XFixes`, Windows `GetCursorInfo`): every sample is the whole
/// pointer state, so it needs none of [`CursorAggregate`]'s per-output
/// reasoning. Keeps the desktop's image ids stable, ignores samples that
/// change nothing, and rate-limits new images
/// ([`MAX_NEW_CURSOR_IMAGES_PER_SECOND`]), keeping only the newest sample
/// while it waits.
#[derive(Debug, Default)]
pub struct HostCursor {
    registry: CursorImageRegistry,
    output: u32,
    generation: u32,
    current: Option<CursorFacts>,
    pending: Option<HostCursorSample>,
    new_images: VecDeque<Instant>,
    samples: u64,
    deferred: u64,
    refused: u64,
}

impl HostCursor {
    /// The pixel space of later samples: the host's output number and its
    /// generation (0 and 0 for a single desktop).
    pub fn set_output(&mut self, output: u32, generation: u32) {
        self.output = output;
        self.generation = generation;
    }

    /// Applies one sample read at `now`.
    pub fn offer(&mut self, sample: HostCursorSample, now: Instant) -> HostCursorOutcome {
        self.samples += 1;
        self.pending = None;
        let next = match &sample {
            HostCursorSample::Hidden => CursorFacts::Hidden {
                output: self.output,
                generation: self.generation,
            },
            HostCursorSample::Image { image, fallback } => {
                if !self.registry.contains(image) {
                    let window = std::time::Duration::from_secs(1);
                    while self
                        .new_images
                        .front()
                        .is_some_and(|at| now.saturating_duration_since(*at) >= window)
                    {
                        self.new_images.pop_front();
                    }
                    if self.new_images.len() >= MAX_NEW_CURSOR_IMAGES_PER_SECOND {
                        self.deferred += 1;
                        self.pending = Some(sample);
                        return HostCursorOutcome::Deferred;
                    }
                    self.new_images.push_back(now);
                }
                match self.registry.register(image) {
                    Ok(Some((id, wire))) => CursorFacts::Image {
                        output: self.output,
                        generation: self.generation,
                        id,
                        wire,
                        fallback: *fallback,
                    },
                    Ok(None) => CursorFacts::Hidden {
                        output: self.output,
                        generation: self.generation,
                    },
                    Err(error) => {
                        self.refused += 1;
                        return HostCursorOutcome::ImageRefused(error);
                    }
                }
            }
        };
        if self.current.as_ref() == Some(&next) {
            HostCursorOutcome::Unchanged
        } else {
            self.current = Some(next);
            HostCursorOutcome::Changed
        }
    }

    /// Applies a deferred sample once its turn came.
    pub fn poll(&mut self, now: Instant) -> HostCursorOutcome {
        match self.retry_at() {
            Some(at) if now >= at => match self.pending.take() {
                Some(sample) => self.offer(sample, now),
                None => HostCursorOutcome::Unchanged,
            },
            _ => HostCursorOutcome::Unchanged,
        }
    }

    /// When a deferred sample may be applied, if one waits.
    #[must_use]
    pub fn retry_at(&self) -> Option<Instant> {
        self.pending.as_ref()?;
        let oldest = self.new_images.front()?;
        Some(*oldest + std::time::Duration::from_secs(1))
    }

    /// The current cursor, once a sample was applied.
    #[must_use]
    pub const fn current(&self) -> Option<&CursorFacts> {
        self.current.as_ref()
    }

    /// Samples offered.
    #[must_use]
    pub const fn samples(&self) -> u64 {
        self.samples
    }

    /// Samples that had to wait for the new-image budget.
    #[must_use]
    pub const fn deferred(&self) -> u64 {
        self.deferred
    }

    /// Images that could not be fitted to the wire.
    #[must_use]
    pub const fn refused_images(&self) -> u64 {
        self.refused
    }

    /// Distinct images remembered for this desktop.
    #[must_use]
    pub fn registered_images(&self) -> usize {
        self.registry.len()
    }
}

/// One desktop's cursor across its output streams, addressed by the
/// compositor's connector names (`Meta-0`, …): the [`CursorAggregate`]
/// plus a stable output number per connector, in first-seen order.
#[derive(Debug, Default)]
pub struct DesktopCursor {
    aggregate: CursorAggregate,
    connectors: Vec<String>,
    events: u64,
    refused_images: u64,
}

impl DesktopCursor {
    /// Applies one capture event of the stream recording `connector` in
    /// output generation `generation`.
    pub fn apply(
        &mut self,
        connector: &str,
        generation: u64,
        event: &CapencCursorEvent,
    ) -> CursorAggregateOutcome {
        self.events += 1;
        let output = self
            .connectors
            .iter()
            .position(|known| known == connector)
            .unwrap_or_else(|| {
                self.connectors.push(connector.to_owned());
                self.connectors.len() - 1
            });
        let outcome = self.aggregate.apply(
            u32::try_from(output).unwrap_or(u32::MAX),
            u32::try_from(generation).unwrap_or(u32::MAX),
            event,
        );
        if matches!(outcome, CursorAggregateOutcome::ImageRefused(_)) {
            self.refused_images += 1;
        }
        outcome
    }

    /// The current cursor.
    #[must_use]
    pub const fn current(&self) -> Option<&CursorFacts> {
        self.aggregate.current()
    }

    /// Capture events applied.
    #[must_use]
    pub const fn events(&self) -> u64 {
        self.events
    }

    /// Images that could not be fitted to the wire.
    #[must_use]
    pub const fn refused_images(&self) -> u64 {
        self.refused_images
    }

    /// See [`CursorAggregate::hidden_while_crossing`].
    #[must_use]
    pub fn hidden_while_crossing(&self) -> bool {
        self.aggregate.hidden_while_crossing()
    }
}

impl CursorFacts {
    /// A one-line description for logs.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Hidden { output, .. } => format!("hidden on output {output}"),
            Self::Image {
                output, id, wire, ..
            } => format!(
                "image {} {}x{} hotspot {:?} png {} B on output {output}",
                id.get(),
                wire.image.width(),
                wire.image.height(),
                wire.image.hotspot(),
                wire.png.len()
            ),
        }
    }
}

/// A record ready to send, and what sending it commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCursorUpdate {
    /// The encoded `CursorUpdate`.
    pub bytes: Vec<u8>,
    /// Whether the image travels inline.
    pub inline_image: bool,
    sequence: u64,
    facts: CursorFacts,
    image: Option<(CursorImageId, usize)>,
}

/// The record for `facts` and, for an image, its id and cache weight.
fn cursor_update_for(
    facts: &CursorFacts,
    sequence: u64,
    inline: bool,
) -> Result<(CursorUpdate, Option<(CursorImageId, usize)>), CursorUpdateError> {
    Ok(match facts {
        CursorFacts::Hidden { output, generation } => (
            CursorUpdate {
                sequence,
                output: *output,
                generation: *generation,
                fallback: CursorShapeKind::Hidden,
                appearance: CursorAppearance::Hidden,
            },
            None,
        ),
        CursorFacts::Image {
            output,
            generation,
            id,
            wire,
            fallback,
        } => {
            let dimension =
                |value: u32| u16::try_from(value).map_err(|_| CursorUpdateError::Dimensions);
            let width = dimension(wire.image.width())?;
            let height = dimension(wire.image.height())?;
            let (hotspot_x, hotspot_y) = wire.image.hotspot();
            (
                CursorUpdate {
                    sequence,
                    output: *output,
                    generation: *generation,
                    fallback: *fallback,
                    appearance: CursorAppearance::Image(CursorImageRef {
                        id: *id,
                        width,
                        height,
                        hotspot_x: dimension(hotspot_x)?,
                        hotspot_y: dimension(hotspot_y)?,
                        inline_png: inline.then(|| wire.png.clone()),
                    }),
                },
                Some((*id, CursorImageCache::<()>::weight(width, height))),
            )
        }
    })
}

/// The record a session agent hands its Pier's relay
/// ([`arcen_protocol::cursor::CursorRecordRelay`]): the whole state with
/// any image inline, because the agent cannot see which images the Deck
/// holds.
///
/// # Errors
///
/// [`CursorUpdateError`] only when a registered image breaks the wire
/// contract, which the registry prevents.
pub fn agent_cursor_record(
    facts: &CursorFacts,
    sequence: u64,
) -> Result<Vec<u8>, CursorUpdateError> {
    let (update, _) = cursor_update_for(facts, sequence, true)?;
    encode_cursor_update(&update)
}

/// The newest cursor state for one attachment's input/cursor lane.
///
/// Only the latest state is kept: a newer state replaces one that has not
/// been sent. A prepared record changes nothing until [`Self::commit`],
/// which the caller runs only after the lane accepted the record, so a
/// refused send never desynchronises the mirror of the Deck's cache.
#[derive(Debug, Default)]
pub struct CursorLaneOutbox {
    mirror: CursorImageCache<()>,
    sequence: u64,
    latest: Option<CursorFacts>,
    sent: Option<CursorFacts>,
}

impl CursorLaneOutbox {
    /// Records the desktop's current cursor.
    pub fn set(&mut self, facts: CursorFacts) {
        self.latest = Some(facts);
    }

    /// Whether a state waits to be sent.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.latest.is_some() && self.latest != self.sent
    }

    /// The lane was (re)opened: the Deck holds no image, and the current
    /// state is sent again.
    pub fn reset_lane(&mut self) {
        self.mirror.clear();
        self.sent = None;
    }

    /// Builds the record for the pending state.
    ///
    /// # Errors
    ///
    /// [`CursorUpdateError`] only when a registered image breaks the wire
    /// contract, which the registry prevents.
    pub fn prepare(&self) -> Result<Option<PreparedCursorUpdate>, CursorUpdateError> {
        if !self.is_pending() {
            return Ok(None);
        }
        let Some(facts) = self.latest.clone() else {
            return Ok(None);
        };
        let sequence = self.sequence + 1;
        let inline_image = match &facts {
            CursorFacts::Hidden { .. } => false,
            CursorFacts::Image { id, .. } => !self.mirror.contains(*id),
        };
        let (update, image) = cursor_update_for(&facts, sequence, inline_image)?;
        Ok(Some(PreparedCursorUpdate {
            bytes: encode_cursor_update(&update)?,
            inline_image,
            sequence,
            facts,
            image,
        }))
    }

    /// Commits a record the lane accepted.
    pub fn commit(&mut self, prepared: PreparedCursorUpdate) {
        self.sequence = prepared.sequence;
        if let Some((id, weight)) = prepared.image {
            if self.mirror.touch(id).is_none() {
                self.mirror.insert(id, weight, ());
            }
        }
        self.sent = Some(prepared.facts);
    }

    /// Ids the Deck holds, least recently used first (diagnostics, tests).
    pub fn mirrored_ids(&self) -> impl Iterator<Item = CursorImageId> + '_ {
        self.mirror.ids()
    }
}

/// What the Deck shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeckCursor {
    /// No record yet: the client's ordinary arrow.
    Default,
    /// The host hides the pointer.
    Hidden,
    /// The host's image.
    Image {
        /// Image id.
        id: CursorImageId,
        /// The image, in stream pixels of `output`.
        image: CursorImage,
        /// Output whose pixel space the image is in.
        output: u32,
        /// That output's generation.
        generation: u32,
    },
    /// A named shape (an image the Deck could not decode or does not hold).
    Named(CursorShapeKind),
}

/// Why the Deck dropped a cursor record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckCursorError {
    /// The record is malformed.
    Record(CursorUpdateError),
}

impl std::fmt::Display for DeckCursorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Record(error) => write!(formatter, "cursor record dropped: {error}"),
        }
    }
}

impl std::error::Error for DeckCursorError {}

/// The Deck's side: applies `CursorUpdate` records in lane order.
#[derive(Debug, Default)]
pub struct DeckCursorState {
    cache: CursorImageCache<Option<CursorImage>>,
    last_sequence: u64,
    current: Option<DeckCursor>,
    undecodable_images: u64,
    unknown_images: u64,
}

impl DeckCursorState {
    /// Applies one record; returns whether what the Deck shows changed.
    ///
    /// # Errors
    ///
    /// [`DeckCursorError`] for a malformed record, which is dropped.
    pub fn apply(&mut self, bytes: &[u8]) -> Result<bool, DeckCursorError> {
        let update = decode_cursor_update(bytes).map_err(DeckCursorError::Record)?;
        // The cache follows every delivered record, even a stale one, so it
        // stays identical to the host's mirror.
        let shown = match update.appearance {
            CursorAppearance::Hidden => DeckCursor::Hidden,
            CursorAppearance::Image(reference) => {
                let weight = CursorImageCache::<()>::weight(reference.width, reference.height);
                let image = match reference.inline_png {
                    Some(png) => {
                        let decoded = CursorImage::from_wire_png(
                            &png,
                            u32::from(reference.width),
                            u32::from(reference.height),
                            (
                                u32::from(reference.hotspot_x),
                                u32::from(reference.hotspot_y),
                            ),
                        )
                        .ok();
                        if decoded.is_none() {
                            self.undecodable_images += 1;
                        }
                        self.cache.insert(reference.id, weight, decoded.clone());
                        decoded
                    }
                    None => {
                        if let Some(held) = self.cache.touch(reference.id) {
                            held.clone()
                        } else {
                            self.unknown_images += 1;
                            None
                        }
                    }
                };
                match image {
                    Some(image) => DeckCursor::Image {
                        id: reference.id,
                        image,
                        output: update.output,
                        generation: update.generation,
                    },
                    None => DeckCursor::Named(update.fallback),
                }
            }
        };
        if update.sequence <= self.last_sequence {
            return Ok(false);
        }
        self.last_sequence = update.sequence;
        let changed = self.current.as_ref() != Some(&shown);
        self.current = Some(shown);
        Ok(changed)
    }

    /// The input/cursor lane was reopened: the host resends its state and
    /// every image.
    pub fn reset_lane(&mut self) {
        self.cache.clear();
        self.last_sequence = 0;
    }

    /// What to show.
    #[must_use]
    pub fn current(&self) -> DeckCursor {
        self.current.clone().unwrap_or(DeckCursor::Default)
    }

    /// Images that arrived but could not be decoded.
    #[must_use]
    pub const fn undecodable_images(&self) -> u64 {
        self.undecodable_images
    }

    /// References to images the Deck did not hold (a broken mirror).
    #[must_use]
    pub const fn unknown_images(&self) -> u64 {
        self.unknown_images
    }

    /// Ids held, least recently used first (diagnostics, tests).
    pub fn cached_ids(&self) -> impl Iterator<Item = CursorImageId> + '_ {
        self.cache.ids()
    }
}

/// Keeps the client's own cursor visible while its pen drives the pointer.
///
/// A host hides its pointer when a tablet tool takes over: Mutter hides it on
/// every tablet event (its tablet tool has a cursor of its own that screen
/// sharing never reports), so a desktop the pen is drawing on reports
/// "hidden" (measured: no cursor in 91% of frames on GNOME 51). That hide
/// answers the client's own pen, not an application. In Tablet support
/// (light) the pen moves the client's system cursor, and the client keeps
/// showing it, with the last shape the host showed:
///
/// - while the client's pen is in proximity, whatever the host reports;
/// - after the pen leaves, while a hide that began during the pen lasts
///   (Mutter shows its pointer again only on pointer input).
///
/// A hide that began without the pen (an application hiding the pointer
/// while you type) is honoured as soon as the pen leaves. The trade-off: one
/// that begins while the pen hovers is not honoured until the host shows a
/// cursor again.
#[derive(Debug, Clone, Default)]
pub struct LocalPenCursor {
    last_visible: Option<DeckCursor>,
    last_visible_shape: Option<CursorShapeKind>,
    hidden_by_pen: bool,
}

impl LocalPenCursor {
    /// Notes a cursor the host reported (images and metadata).
    pub fn observe(&mut self, host: &DeckCursor, local_pen_drives: bool) {
        match host {
            DeckCursor::Hidden => self.note_hidden(local_pen_drives),
            visible => {
                self.last_visible = Some(visible.clone());
                self.hidden_by_pen = false;
            }
        }
    }

    /// Notes a named shape the host reported (shape-only hosts).
    pub fn observe_shape(&mut self, shape: CursorShapeKind, local_pen_drives: bool) {
        if shape == CursorShapeKind::Hidden {
            self.note_hidden(local_pen_drives);
        } else {
            self.last_visible_shape = Some(shape);
            self.hidden_by_pen = false;
        }
    }

    fn note_hidden(&mut self, local_pen_drives: bool) {
        if local_pen_drives {
            self.hidden_by_pen = true;
        }
    }

    fn keeps_cursor(&self, local_pen_drives: bool) -> bool {
        local_pen_drives || self.hidden_by_pen
    }

    /// What the client shows for `host`.
    #[must_use]
    pub fn shown(&self, host: &DeckCursor, local_pen_drives: bool) -> DeckCursor {
        match host {
            DeckCursor::Hidden if self.keeps_cursor(local_pen_drives) => {
                self.last_visible.clone().unwrap_or(DeckCursor::Default)
            }
            other => other.clone(),
        }
    }

    /// The named shape the client shows for `shape`.
    #[must_use]
    pub fn shown_shape(&self, shape: CursorShapeKind, local_pen_drives: bool) -> CursorShapeKind {
        if shape == CursorShapeKind::Hidden && self.keeps_cursor(local_pen_drives) {
            self.last_visible_shape.unwrap_or_default()
        } else {
            shape
        }
    }

    /// A new connection: nothing is known about the host's cursor.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// The named shape closest to an X cursor name (Xcursor theme, core
/// cursor font or CSS name), matched case-insensitively. An empty or
/// unknown name is `default`: whether a cursor is invisible is decided from
/// its pixels, not its name, except for the explicitly blank names.
#[must_use]
pub fn shape_for_x_cursor_name(name: &str) -> CursorShapeKind {
    match name.to_ascii_lowercase().as_str() {
        "n-resize" | "s-resize" | "ns-resize" | "size_ver" | "top_side" | "bottom_side"
        | "v_double_arrow" | "row-resize" | "sb_v_double_arrow" => CursorShapeKind::ResizeNs,
        "e-resize" | "w-resize" | "ew-resize" | "size_hor" | "right_side" | "left_side"
        | "h_double_arrow" | "col-resize" | "sb_h_double_arrow" => CursorShapeKind::ResizeEw,
        "nw-resize"
        | "se-resize"
        | "nwse-resize"
        | "size_fdiag"
        | "top_left_corner"
        | "bottom_right_corner"
        | "fd_double_arrow"
        | "ul_angle"
        | "lr_angle" => CursorShapeKind::ResizeNwse,
        "ne-resize" | "sw-resize" | "nesw-resize" | "size_bdiag" | "top_right_corner"
        | "bottom_left_corner" | "bd_double_arrow" | "ur_angle" | "ll_angle" => {
            CursorShapeKind::ResizeNesw
        }
        "move" | "size_all" | "fleur" | "all-scroll" => CursorShapeKind::ResizeAll,
        "text" | "xterm" | "ibeam" | "vertical-text" => CursorShapeKind::Text,
        "pointer" | "hand" | "hand1" | "hand2" | "pointing_hand" => CursorShapeKind::Pointer,
        "crosshair" | "cross" | "tcross" | "cross_reverse" | "diamond_cross" => {
            CursorShapeKind::Crosshair
        }
        "grab" | "openhand" | "hand3" => CursorShapeKind::Grab,
        "grabbing" | "closedhand" | "hand4" | "dnd-move" => CursorShapeKind::Grabbing,
        "zoom-in" | "zoom_in" => CursorShapeKind::ZoomIn,
        "zoom-out" | "zoom_out" => CursorShapeKind::ZoomOut,
        "wait" | "watch" | "clock" => CursorShapeKind::Wait,
        "progress" | "left_ptr_watch" | "half-busy" => CursorShapeKind::Progress,
        "help" | "question_arrow" | "whats_this" => CursorShapeKind::Help,
        "not-allowed" | "forbidden" | "circle" | "no-drop" | "dnd-no-drop" | "crossed_circle" => {
            CursorShapeKind::NotAllowed
        }
        "none" | "blank_cursor" | "nil_cursor" => CursorShapeKind::Hidden,
        _ => CursorShapeKind::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(seed: u8, side: u32) -> CursorImage {
        let mut rgba = Vec::new();
        for index in 0..side * side {
            rgba.extend_from_slice(&[seed, (index % 251) as u8, 7, 255]);
        }
        CursorImage::from_straight_rgba(side, side, (1, 1), rgba).unwrap()
    }

    fn event(on_output: bool, sprite: CapencCursorSprite) -> CapencCursorEvent {
        CapencCursorEvent { on_output, sprite }
    }

    fn send(outbox: &mut CursorLaneOutbox, deck: &mut DeckCursorState) -> Option<bool> {
        let prepared = outbox.prepare().unwrap()?;
        let inline = prepared.inline_image;
        deck.apply(&prepared.bytes).unwrap();
        outbox.commit(prepared);
        Some(inline)
    }

    #[test]
    fn aggregate_follows_mutter_and_gnome_remote_desktop() {
        let mut aggregate = CursorAggregate::default();
        // Position-only before any sprite: nothing to show yet.
        assert_eq!(
            aggregate.apply(0, 1, &event(true, CapencCursorSprite::Unchanged)),
            CursorAggregateOutcome::Unchanged
        );
        assert!(aggregate.current().is_none());
        // An empty sprite hides the pointer.
        assert_eq!(
            aggregate.apply(0, 1, &event(true, CapencCursorSprite::Empty)),
            CursorAggregateOutcome::Changed
        );
        assert!(matches!(
            aggregate.current(),
            Some(CursorFacts::Hidden { .. })
        ));
        // A sprite shows it.
        aggregate.apply(0, 1, &event(true, CapencCursorSprite::Image(image(1, 8))));
        let Some(CursorFacts::Image { id, .. }) = aggregate.current().cloned() else {
            panic!("expected an image");
        };
        // Mutter id 0 on the only output: a client set no cursor surface.
        assert_eq!(
            aggregate.apply(0, 1, &event(false, CapencCursorSprite::Unchanged)),
            CursorAggregateOutcome::Changed
        );
        assert!(!aggregate.hidden_while_crossing());
        assert!(matches!(
            aggregate.current(),
            Some(CursorFacts::Hidden { .. })
        ));
        // Back with positions only: the same sprite.
        aggregate.apply(0, 1, &event(true, CapencCursorSprite::Unchanged));
        assert!(
            matches!(aggregate.current(), Some(CursorFacts::Image { id: back, .. }) if *back == id)
        );
        // The same image again keeps its id.
        aggregate.apply(0, 1, &event(true, CapencCursorSprite::Image(image(1, 8))));
        assert!(
            matches!(aggregate.current(), Some(CursorFacts::Image { id: again, .. }) if *again == id)
        );
        // A fully transparent sprite is an empty one.
        let clear = CursorImage::from_straight_rgba(2, 2, (0, 0), vec![0; 16]).unwrap();
        aggregate.apply(0, 1, &event(true, CapencCursorSprite::Image(clear)));
        assert!(matches!(
            aggregate.current(),
            Some(CursorFacts::Hidden { .. })
        ));
    }

    #[test]
    fn desktop_cursor_numbers_outputs_by_connector() {
        let mut desktop = DesktopCursor::default();
        desktop.apply(
            "Meta-1",
            3,
            &event(false, CapencCursorSprite::Image(image(2, 8))),
        );
        desktop.apply("Meta-2", 3, &event(true, CapencCursorSprite::Empty));
        assert_eq!(
            desktop.current(),
            Some(&CursorFacts::Hidden {
                output: 1,
                generation: 3
            })
        );
        desktop.apply("Meta-1", 3, &event(true, CapencCursorSprite::Unchanged));
        assert!(matches!(
            desktop.current(),
            Some(CursorFacts::Image { output: 0, .. })
        ));
        assert_eq!(desktop.events(), 3);
        assert!(desktop.current().unwrap().summary().contains("on output 0"));
    }

    #[test]
    fn a_reconnected_capture_reuses_the_remembered_sprite() {
        let mut aggregate = CursorAggregate::default();
        aggregate.apply(0, 1, &event(true, CapencCursorSprite::Image(image(3, 8))));
        let before = aggregate.current().cloned();
        // A new capture epoch on the same stream: Mutter sends positions only.
        assert_eq!(
            aggregate.apply(0, 1, &event(true, CapencCursorSprite::Unchanged)),
            CursorAggregateOutcome::Unchanged
        );
        assert_eq!(aggregate.current().cloned(), before);
    }

    #[test]
    fn the_output_holding_the_cursor_decides_across_outputs() {
        let mut aggregate = CursorAggregate::default();
        aggregate.apply(0, 1, &event(true, CapencCursorSprite::Image(image(1, 8))));
        // Output 1 had an empty sprite while the cursor was elsewhere.
        aggregate.apply(1, 4, &event(false, CapencCursorSprite::Empty));
        assert!(matches!(
            aggregate.current(),
            Some(CursorFacts::Image { output: 0, .. })
        ));
        aggregate.apply(1, 4, &event(true, CapencCursorSprite::Image(image(2, 16))));
        assert!(matches!(
            aggregate.current(),
            Some(CursorFacts::Image {
                output: 1,
                generation: 4,
                ..
            })
        ));
        aggregate.apply(0, 1, &event(false, CapencCursorSprite::Unchanged));
        assert!(matches!(
            aggregate.current(),
            Some(CursorFacts::Image { output: 1, .. })
        ));
        // Neither output shows it any more: hidden, perhaps while crossing.
        aggregate.apply(1, 4, &event(false, CapencCursorSprite::Unchanged));
        assert!(matches!(
            aggregate.current(),
            Some(CursorFacts::Hidden { output: 1, .. })
        ));
        assert!(aggregate.hidden_while_crossing());
        aggregate.apply(1, 4, &event(true, CapencCursorSprite::Unchanged));
        assert!(matches!(
            aggregate.current(),
            Some(CursorFacts::Image { output: 1, .. })
        ));
        // An output whose capture never saw a sprite shows the last one.
        aggregate.apply(2, 4, &event(true, CapencCursorSprite::Unchanged));
        assert!(matches!(
            aggregate.current(),
            Some(CursorFacts::Image { output: 2, .. })
        ));
        aggregate.remove_output(2);
        // Output 1 leaves the topology with the cursor on it; Mutter moves
        // the pointer to an output that remains.
        aggregate.remove_output(1);
        aggregate.apply(0, 5, &event(true, CapencCursorSprite::Unchanged));
        assert!(matches!(
            aggregate.current(),
            Some(CursorFacts::Image {
                output: 0,
                generation: 5,
                ..
            })
        ));
    }

    #[test]
    fn images_travel_once_and_hidden_needs_no_image() {
        let mut aggregate = CursorAggregate::default();
        let mut outbox = CursorLaneOutbox::default();
        let mut deck = DeckCursorState::default();
        assert_eq!(deck.current(), DeckCursor::Default);
        for (index, sprite) in [
            CapencCursorSprite::Image(image(1, 8)),
            CapencCursorSprite::Image(image(2, 8)),
            CapencCursorSprite::Image(image(1, 8)),
            CapencCursorSprite::Empty,
        ]
        .into_iter()
        .enumerate()
        {
            aggregate.apply(0, 1, &event(true, sprite));
            outbox.set(aggregate.current().cloned().unwrap());
            let inline = send(&mut outbox, &mut deck).unwrap();
            assert_eq!(inline, index < 2, "record {index}");
        }
        assert_eq!(deck.current(), DeckCursor::Hidden);
        assert!(outbox.prepare().unwrap().is_none(), "nothing pending");
        // A reopened lane resends the state, and images inline again.
        aggregate.apply(0, 1, &event(true, CapencCursorSprite::Image(image(2, 8))));
        outbox.set(aggregate.current().cloned().unwrap());
        outbox.reset_lane();
        deck.reset_lane();
        assert_eq!(send(&mut outbox, &mut deck), Some(true));
        assert!(matches!(deck.current(), DeckCursor::Image { .. }));
        assert_eq!(deck.unknown_images(), 0);
    }

    #[test]
    fn a_refused_send_commits_nothing() {
        let mut aggregate = CursorAggregate::default();
        let mut outbox = CursorLaneOutbox::default();
        aggregate.apply(0, 1, &event(true, CapencCursorSprite::Image(image(1, 8))));
        outbox.set(aggregate.current().cloned().unwrap());
        let first = outbox.prepare().unwrap().unwrap();
        assert!(first.inline_image);
        // Not committed: still pending, still inline.
        let again = outbox.prepare().unwrap().unwrap();
        assert!(again.inline_image);
        assert_eq!(again, first);
        assert_eq!(outbox.mirrored_ids().count(), 0);
    }

    #[test]
    fn the_host_mirror_matches_the_deck_cache_under_eviction() {
        let mut aggregate = CursorAggregate::default();
        let mut outbox = CursorLaneOutbox::default();
        let mut deck = DeckCursorState::default();
        let mut state = 0x9e37_79b9_u32;
        for step in 0..600 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let choice = state % 40;
            let sprite = if choice == 39 {
                CapencCursorSprite::Empty
            } else {
                // Sizes 8..=136 make the byte budget evict as well.
                let side = 8 + (choice % 5) * 32;
                CapencCursorSprite::Image(image(u8::try_from(choice).unwrap(), side))
            };
            aggregate.apply(0, 1, &event(true, sprite));
            outbox.set(aggregate.current().cloned().unwrap());
            if step % 97 == 0 {
                outbox.reset_lane();
                deck.reset_lane();
            }
            if step % 7 == 3 {
                // A refused send: prepared, not delivered, not committed.
                let _ = outbox.prepare().unwrap();
                continue;
            }
            send(&mut outbox, &mut deck);
            assert!(
                outbox.mirrored_ids().eq(deck.cached_ids()),
                "step {step}: mirror diverged"
            );
        }
        assert_eq!(deck.unknown_images(), 0);
        assert_eq!(deck.undecodable_images(), 0);
        assert!(outbox.mirrored_ids().count() <= CURSOR_CACHE_ENTRIES);
    }

    #[test]
    fn stale_and_malformed_records_do_not_change_the_cursor() {
        let mut aggregate = CursorAggregate::default();
        let mut outbox = CursorLaneOutbox::default();
        let mut deck = DeckCursorState::default();
        aggregate.apply(0, 1, &event(true, CapencCursorSprite::Image(image(1, 8))));
        outbox.set(aggregate.current().cloned().unwrap());
        let first = outbox.prepare().unwrap().unwrap();
        assert!(deck.apply(&first.bytes).unwrap());
        outbox.commit(first.clone());
        assert!(!deck.apply(&first.bytes).unwrap(), "same sequence again");
        assert!(deck.apply(&[0x50, 1, 0]).is_err());
        assert!(matches!(deck.current(), DeckCursor::Image { .. }));
    }

    #[test]
    fn registry_is_bounded_and_never_reuses_ids() {
        let mut registry = CursorImageRegistry::default();
        let mut ids = Vec::new();
        for seed in 0..=200u8 {
            let (id, _) = registry.register(&image(seed, 8)).unwrap().unwrap();
            ids.push(id);
        }
        assert!(registry.len() <= CURSOR_REGISTRY_ENTRIES);
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len());
        // The oldest image was evicted and comes back under a new id.
        let (again, _) = registry.register(&image(0, 8)).unwrap().unwrap();
        assert_ne!(again, ids[0]);
    }

    #[test]
    fn the_local_pen_keeps_the_cursor_through_the_hosts_tablet_hide() {
        let arrow = DeckCursor::Named(CursorShapeKind::Text);
        let mut pen = LocalPenCursor::default();
        pen.observe(&arrow, false);
        // The host hides its pointer because the pen took over.
        pen.observe(&DeckCursor::Hidden, true);
        assert_eq!(pen.shown(&DeckCursor::Hidden, true), arrow);
        // The pen leaves; the host shows its pointer only on pointer input.
        assert_eq!(pen.shown(&DeckCursor::Hidden, false), arrow);
        // The host shows a cursor again: back to following it.
        pen.observe(&DeckCursor::Default, false);
        assert_eq!(pen.shown(&DeckCursor::Default, false), DeckCursor::Default);
    }

    #[test]
    fn a_hide_without_the_pen_is_honoured_once_the_pen_leaves() {
        let mut pen = LocalPenCursor::default();
        pen.observe(&DeckCursor::Default, false);
        // An application hides the pointer while the user types.
        pen.observe(&DeckCursor::Hidden, false);
        assert_eq!(pen.shown(&DeckCursor::Hidden, false), DeckCursor::Hidden);
        // Hovering the pen shows the client's cursor (it is what moves) ...
        assert_eq!(pen.shown(&DeckCursor::Hidden, true), DeckCursor::Default);
        // ... and leaving restores the application's hide.
        assert_eq!(pen.shown(&DeckCursor::Hidden, false), DeckCursor::Hidden);
        pen.reset();
        assert_eq!(pen.shown(&DeckCursor::Hidden, false), DeckCursor::Hidden);
    }

    #[test]
    fn shape_only_hosts_follow_the_same_rule() {
        let mut pen = LocalPenCursor::default();
        pen.observe_shape(CursorShapeKind::Crosshair, false);
        pen.observe_shape(CursorShapeKind::Hidden, true);
        assert_eq!(
            pen.shown_shape(CursorShapeKind::Hidden, false),
            CursorShapeKind::Crosshair
        );
        pen.observe_shape(CursorShapeKind::Text, false);
        pen.observe_shape(CursorShapeKind::Hidden, false);
        assert_eq!(
            pen.shown_shape(CursorShapeKind::Hidden, false),
            CursorShapeKind::Hidden
        );
        assert_eq!(
            pen.shown_shape(CursorShapeKind::Hidden, true),
            CursorShapeKind::Text
        );
    }

    fn sample(seed: u8, fallback: CursorShapeKind) -> HostCursorSample {
        HostCursorSample::Image {
            image: image(seed, 8),
            fallback,
        }
    }

    #[test]
    fn host_cursor_keeps_ids_and_fallbacks_and_ignores_repeats() {
        let now = Instant::now();
        let mut host = HostCursor::default();
        assert_eq!(
            host.offer(sample(1, CursorShapeKind::Crosshair), now),
            HostCursorOutcome::Changed
        );
        let current = host.current().cloned().unwrap();
        assert_eq!(current.fallback_shape(), CursorShapeKind::Crosshair);
        assert_eq!(
            host.offer(sample(1, CursorShapeKind::Crosshair), now),
            HostCursorOutcome::Unchanged
        );
        // The same image under another name is a change for an old Deck.
        assert_eq!(
            host.offer(sample(1, CursorShapeKind::Text), now),
            HostCursorOutcome::Changed
        );
        assert_eq!(host.registered_images(), 1);
        assert_eq!(
            host.offer(HostCursorSample::Hidden, now),
            HostCursorOutcome::Changed
        );
        assert_eq!(
            host.current().map(CursorFacts::fallback_shape),
            Some(CursorShapeKind::Hidden)
        );
        // A fully transparent image hides the pointer.
        let clear = CursorImage::from_straight_rgba(4, 4, (0, 0), vec![0; 64]).unwrap();
        host.offer(sample(1, CursorShapeKind::Default), now);
        assert_eq!(
            host.offer(
                HostCursorSample::Image {
                    image: clear,
                    fallback: CursorShapeKind::Default
                },
                now
            ),
            HostCursorOutcome::Changed
        );
        assert!(matches!(host.current(), Some(CursorFacts::Hidden { .. })));
    }

    #[test]
    fn host_cursor_rate_limits_new_images_but_not_known_ones() {
        let start = Instant::now();
        let mut host = HostCursor::default();
        for seed in 0..MAX_NEW_CURSOR_IMAGES_PER_SECOND {
            let seed = u8::try_from(seed).unwrap();
            let outcome = host.offer(sample(seed, CursorShapeKind::Default), start);
            assert_eq!(outcome, HostCursorOutcome::Changed);
        }
        let later = start + std::time::Duration::from_millis(300);
        assert_eq!(
            host.offer(sample(200, CursorShapeKind::Default), later),
            HostCursorOutcome::Deferred
        );
        // A newer sample replaces the waiting one; a known image is free.
        assert_eq!(
            host.offer(sample(201, CursorShapeKind::Default), later),
            HostCursorOutcome::Deferred
        );
        assert_eq!(
            host.offer(sample(3, CursorShapeKind::Default), later),
            HostCursorOutcome::Changed
        );
        assert!(host.retry_at().is_none(), "a known image cleared the wait");
        host.offer(sample(202, CursorShapeKind::Default), later);
        let retry = host.retry_at().unwrap();
        assert_eq!(retry, start + std::time::Duration::from_secs(1));
        assert_eq!(host.poll(later), HostCursorOutcome::Unchanged);
        assert_eq!(host.poll(retry), HostCursorOutcome::Changed);
        assert!(host.retry_at().is_none());
        assert_eq!(host.deferred(), 3);
        assert_eq!(
            host.registered_images(),
            MAX_NEW_CURSOR_IMAGES_PER_SECOND + 1
        );
    }

    #[test]
    fn agent_records_always_carry_their_image_and_fallback() {
        let mut host = HostCursor::default();
        host.set_output(2, 7);
        host.offer(sample(9, CursorShapeKind::ZoomIn), Instant::now());
        let facts = host.current().cloned().unwrap();
        let record = agent_cursor_record(&facts, 5).unwrap();
        let update = decode_cursor_update(&record).unwrap();
        assert_eq!(
            (
                update.sequence,
                update.output,
                update.generation,
                update.fallback
            ),
            (5, 2, 7, CursorShapeKind::ZoomIn)
        );
        let CursorAppearance::Image(reference) = update.appearance else {
            panic!("expected an image");
        };
        assert!(reference.inline_png.is_some());
        // Through the relay the Deck shows it, and only once inline.
        let mut relay = arcen_protocol::cursor::CursorRecordRelay::default();
        let mut deck = DeckCursorState::default();
        relay.offer(&record).unwrap();
        let prepared = relay.prepare(1).unwrap();
        assert!(prepared.inline_image);
        assert!(deck.apply(&prepared.bytes).unwrap());
        relay.commit(prepared);
        assert!(matches!(deck.current(), DeckCursor::Image { .. }));
        relay
            .offer(&agent_cursor_record(&facts, 6).unwrap())
            .unwrap();
        let prepared = relay.prepare(1).unwrap();
        assert!(!prepared.inline_image);
        deck.apply(&prepared.bytes).unwrap();
        assert_eq!(deck.unknown_images(), 0);
    }

    #[test]
    fn x_cursor_names_map_to_shapes() {
        for (name, shape) in [
            ("left_ptr", CursorShapeKind::Default),
            ("", CursorShapeKind::Default),
            ("XTERM", CursorShapeKind::Text),
            ("hand2", CursorShapeKind::Pointer),
            ("crosshair", CursorShapeKind::Crosshair),
            ("ul_angle", CursorShapeKind::ResizeNwse),
            ("ll_angle", CursorShapeKind::ResizeNesw),
            ("fleur", CursorShapeKind::ResizeAll),
            ("left_ptr_watch", CursorShapeKind::Progress),
            ("watch", CursorShapeKind::Wait),
            ("not-allowed", CursorShapeKind::NotAllowed),
            ("blank_cursor", CursorShapeKind::Hidden),
            ("some_unknown_cursor_2099", CursorShapeKind::Default),
        ] {
            assert_eq!(shape_for_x_cursor_name(name), shape, "{name}");
        }
    }
}
