use std::mem;

use arcen_keel::{BgraFrame, BlockGrid, DamageMap};

use crate::{
    DeliveryMode, DeliveryStatus, FRAME_HEADER_BYTES, FrameKind, MAX_PATCHES_PER_FRAME, ModelKind,
    PATCH_DESCRIPTOR_BYTES, PATCH_FALLBACK_BASIS_POINTS, usize_to_u64,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FrameDisposition {
    Suppress,
    Emit(FrameKind),
}

impl FrameDisposition {
    pub(crate) const fn frame_kind(self) -> Option<FrameKind> {
        match self {
            Self::Suppress => None,
            Self::Emit(kind) => Some(kind),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ModelFrameStats {
    pub(crate) frame_kind: FrameKind,
    pub(crate) delivery: DeliveryStatus,
    pub(crate) carrier_bytes: u64,
    pub(crate) source_copy_bytes: u64,
    pub(crate) source_copy_operations: u64,
    pub(crate) compositor_copy_bytes: u64,
    pub(crate) compositor_copy_operations: u64,
    pub(crate) patch_count: usize,
    pub(crate) full_frame_fallback: bool,
    pub(crate) allocation_growths: u64,
}

impl ModelFrameStats {
    const fn new(frame_kind: FrameKind) -> Self {
        Self {
            frame_kind,
            delivery: DeliveryStatus::Applied,
            carrier_bytes: 0,
            source_copy_bytes: 0,
            source_copy_operations: 0,
            compositor_copy_bytes: 0,
            compositor_copy_operations: 0,
            patch_count: 0,
            full_frame_fallback: false,
            allocation_growths: 0,
        }
    }
}

#[derive(Debug)]
pub(crate) enum FramingModel {
    Full(CompleteFrameModel),
    Rows(CompleteFrameModel),
    Rects(RectFrameModel),
    Patches(PatchModel),
}

impl FramingModel {
    pub(crate) fn new(kind: ModelKind, grid: BlockGrid, frame_bytes: usize) -> Self {
        match kind {
            ModelKind::FullPicture => {
                Self::Full(CompleteFrameModel::new(grid, frame_bytes, CopyShape::Full))
            }
            ModelKind::DirtyRows => {
                Self::Rows(CompleteFrameModel::new(grid, frame_bytes, CopyShape::Rows))
            }
            ModelKind::DirtyRects => Self::Rects(RectFrameModel::new(grid, frame_bytes)),
            ModelKind::BoundedPatches => Self::Patches(PatchModel::new(grid, frame_bytes)),
        }
    }

    pub(crate) fn process(
        &mut self,
        frame: BgraFrame<'_>,
        damage: DamageMap<'_>,
        frame_kind: FrameKind,
        sequence: u64,
        delivery: DeliveryMode,
    ) -> ModelFrameStats {
        match self {
            Self::Full(model) | Self::Rows(model) => model.process(frame, damage, frame_kind),
            Self::Rects(model) => model.process(frame, damage, frame_kind),
            Self::Patches(model) => model.process(frame, damage, frame_kind, sequence, delivery),
        }
    }

    pub(crate) fn reconstructed(&self) -> &[u8] {
        match self {
            Self::Full(model) | Self::Rows(model) => &model.retained,
            Self::Rects(model) => &model.retained,
            Self::Patches(model) => &model.receiver.pixels,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CopyShape {
    Full,
    Rows,
}

#[derive(Debug)]
pub(crate) struct CompleteFrameModel {
    grid: BlockGrid,
    retained: Vec<u8>,
    shape: CopyShape,
}

impl CompleteFrameModel {
    fn new(grid: BlockGrid, frame_bytes: usize, shape: CopyShape) -> Self {
        Self {
            grid,
            retained: vec![0; frame_bytes],
            shape,
        }
    }

    fn process(
        &mut self,
        frame: BgraFrame<'_>,
        damage: DamageMap<'_>,
        frame_kind: FrameKind,
    ) -> ModelFrameStats {
        let mut stats = ModelFrameStats::new(frame_kind);
        if frame_kind != FrameKind::Keepalive {
            match (frame_kind, self.shape) {
                (FrameKind::Keyframe, _) | (_, CopyShape::Full) => {
                    copy_full_frame(frame, &mut self.retained, &mut stats, CopySide::Source);
                }
                (FrameKind::Delta, CopyShape::Rows) => {
                    for rows in damage.dirty_block_rows() {
                        copy_full_width_rows(
                            frame,
                            &mut self.retained,
                            rows.start,
                            rows.end,
                            &mut stats,
                            CopySide::Source,
                        );
                    }
                }
                (FrameKind::Keepalive, CopyShape::Rows) => {}
            }
        }
        stats.carrier_bytes = usize_to_u64(self.retained.len().saturating_add(FRAME_HEADER_BYTES));
        debug_assert_eq!(damage.grid(), self.grid);
        stats
    }
}

#[derive(Debug)]
pub(crate) struct RectFrameModel {
    retained: Vec<u8>,
    scratch: RectScratch,
}

impl RectFrameModel {
    fn new(grid: BlockGrid, frame_bytes: usize) -> Self {
        Self {
            retained: vec![0; frame_bytes],
            scratch: RectScratch::new(grid),
        }
    }

    fn process(
        &mut self,
        frame: BgraFrame<'_>,
        damage: DamageMap<'_>,
        frame_kind: FrameKind,
    ) -> ModelFrameStats {
        let mut stats = ModelFrameStats::new(frame_kind);
        match frame_kind {
            FrameKind::Keyframe => {
                copy_full_frame(frame, &mut self.retained, &mut stats, CopySide::Source);
            }
            FrameKind::Delta => {
                let rects = self.scratch.build(damage, &mut stats);
                for rect in rects {
                    copy_rect_to_tight_frame(
                        frame,
                        &mut self.retained,
                        *rect,
                        &mut stats,
                        CopySide::Source,
                    );
                }
            }
            FrameKind::Keepalive => {}
        }
        stats.carrier_bytes = usize_to_u64(self.retained.len().saturating_add(FRAME_HEADER_BYTES));
        stats
    }
}

#[derive(Debug)]
pub(crate) struct PatchModel {
    frame_bytes: usize,
    scratch: RectScratch,
    payload: Vec<u8>,
    records: Vec<PatchRecord>,
    receiver: PatchReceiver,
}

impl PatchModel {
    fn new(grid: BlockGrid, frame_bytes: usize) -> Self {
        Self {
            frame_bytes,
            scratch: RectScratch::new(grid),
            payload: Vec::with_capacity(frame_bytes),
            records: Vec::with_capacity(MAX_PATCHES_PER_FRAME),
            receiver: PatchReceiver::new(grid, frame_bytes),
        }
    }

    fn process(
        &mut self,
        frame: BgraFrame<'_>,
        damage: DamageMap<'_>,
        requested_kind: FrameKind,
        sequence: u64,
        delivery: DeliveryMode,
    ) -> ModelFrameStats {
        let mut stats = ModelFrameStats::new(requested_kind);
        self.payload.clear();
        self.records.clear();

        let mut actual_kind = requested_kind;
        if requested_kind == FrameKind::Keyframe {
            self.encode_full(frame, &mut stats);
        } else if requested_kind == FrameKind::Delta {
            let rects = self.scratch.build(damage, &mut stats);
            let payload_bytes = rects.iter().fold(0usize, |total, rect| {
                total.saturating_add(rect.pixel_bytes())
            });
            let patch_bytes = FRAME_HEADER_BYTES
                .saturating_add(rects.len().saturating_mul(PATCH_DESCRIPTOR_BYTES))
                .saturating_add(payload_bytes);
            let full_bytes = FRAME_HEADER_BYTES
                .saturating_add(PATCH_DESCRIPTOR_BYTES)
                .saturating_add(self.frame_bytes);
            let fallback_for_size = (patch_bytes as u128).saturating_mul(10_000)
                >= (full_bytes as u128).saturating_mul(u128::from(PATCH_FALLBACK_BASIS_POINTS));
            if rects.len() > MAX_PATCHES_PER_FRAME || fallback_for_size {
                actual_kind = FrameKind::Keyframe;
                stats.full_frame_fallback = true;
                self.encode_full(frame, &mut stats);
            } else {
                encode_rects(
                    frame,
                    rects,
                    &mut self.payload,
                    &mut self.records,
                    &mut stats,
                );
            }
        }

        stats.frame_kind = actual_kind;
        stats.patch_count = self.records.len();
        stats.carrier_bytes = usize_to_u64(
            FRAME_HEADER_BYTES
                .saturating_add(self.records.len().saturating_mul(PATCH_DESCRIPTOR_BYTES))
                .saturating_add(self.payload.len()),
        );
        stats.delivery = match delivery {
            DeliveryMode::DropFrame => DeliveryStatus::Dropped,
            DeliveryMode::InOrder | DeliveryMode::ReversePatches => self.receiver.apply(
                sequence,
                actual_kind,
                &self.records,
                &self.payload,
                delivery,
                &mut stats,
            ),
        };
        stats
    }

    fn encode_full(&mut self, frame: BgraFrame<'_>, stats: &mut ModelFrameStats) {
        let rect = PixelRect {
            x: 0,
            y: 0,
            width: frame.grid().width(),
            height: frame.grid().height(),
        };
        encode_rects(
            frame,
            std::slice::from_ref(&rect),
            &mut self.payload,
            &mut self.records,
            stats,
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PatchRecord {
    rect: PixelRect,
    payload_offset: usize,
    payload_len: usize,
}

#[derive(Debug)]
struct PatchReceiver {
    grid: BlockGrid,
    pixels: Vec<u8>,
    expected_sequence: Option<u64>,
    synchronized: bool,
}

impl PatchReceiver {
    fn new(grid: BlockGrid, frame_bytes: usize) -> Self {
        Self {
            grid,
            pixels: vec![0; frame_bytes],
            expected_sequence: None,
            synchronized: false,
        }
    }

    fn apply(
        &mut self,
        sequence: u64,
        frame_kind: FrameKind,
        records: &[PatchRecord],
        payload: &[u8],
        delivery: DeliveryMode,
        stats: &mut ModelFrameStats,
    ) -> DeliveryStatus {
        if frame_kind != FrameKind::Keyframe
            && (!self.synchronized || self.expected_sequence != Some(sequence))
        {
            self.synchronized = false;
            return DeliveryStatus::RejectedSequenceGap;
        }

        if delivery == DeliveryMode::ReversePatches {
            for record in records.iter().rev() {
                apply_record(self.grid, &mut self.pixels, payload, *record, stats);
            }
        } else {
            for record in records {
                apply_record(self.grid, &mut self.pixels, payload, *record, stats);
            }
        }
        self.expected_sequence = Some(sequence.saturating_add(1));
        self.synchronized = true;
        if delivery == DeliveryMode::ReversePatches {
            DeliveryStatus::ReorderedApplied
        } else {
            DeliveryStatus::Applied
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PixelRect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl PixelRect {
    fn pixel_bytes(self) -> usize {
        self.width
            .saturating_mul(self.height)
            .saturating_mul(crate::BGRA_BYTES_PER_PIXEL)
    }

    fn bottom(self) -> usize {
        self.y.saturating_add(self.height)
    }
}

#[derive(Debug)]
struct RectScratch {
    grid: BlockGrid,
    rects: Vec<PixelRect>,
    active: Vec<usize>,
    next_active: Vec<usize>,
}

impl RectScratch {
    fn new(grid: BlockGrid) -> Self {
        Self {
            grid,
            rects: Vec::with_capacity(grid.block_count()),
            active: Vec::with_capacity(grid.blocks_wide()),
            next_active: Vec::with_capacity(grid.blocks_wide()),
        }
    }

    fn build<'a>(
        &'a mut self,
        damage: DamageMap<'_>,
        stats: &mut ModelFrameStats,
    ) -> &'a [PixelRect] {
        debug_assert_eq!(damage.grid(), self.grid);
        self.rects.clear();
        self.active.clear();
        self.next_active.clear();

        for block_y in 0..self.grid.blocks_tall() {
            self.next_active.clear();
            let Some(row_range) = self.grid.block_row_pixels(block_y) else {
                continue;
            };
            let mut block_x = 0usize;
            while block_x < self.grid.blocks_wide() {
                while block_x < self.grid.blocks_wide()
                    && !block_is_dirty(damage, self.grid, block_x, block_y)
                {
                    block_x += 1;
                }
                if block_x == self.grid.blocks_wide() {
                    break;
                }
                let first = block_x;
                while block_x < self.grid.blocks_wide()
                    && block_is_dirty(damage, self.grid, block_x, block_y)
                {
                    block_x += 1;
                }
                let last = block_x - 1;
                let Some(first_bounds) = self
                    .grid
                    .block_index(first, block_y)
                    .and_then(|index| self.grid.block_bounds(index))
                else {
                    continue;
                };
                let Some(last_bounds) = self
                    .grid
                    .block_index(last, block_y)
                    .and_then(|index| self.grid.block_bounds(index))
                else {
                    continue;
                };
                let x = first_bounds.x;
                let width = last_bounds
                    .x
                    .saturating_add(last_bounds.width)
                    .saturating_sub(x);
                let matching = self.active.iter().copied().find(|index| {
                    let rect = self.rects[*index];
                    rect.x == x && rect.width == width && rect.bottom() == row_range.start
                });
                let index = if let Some(index) = matching {
                    self.rects[index].height = self.rects[index]
                        .height
                        .saturating_add(row_range.end.saturating_sub(row_range.start));
                    index
                } else {
                    let index = self.rects.len();
                    push_reserved(
                        &mut self.rects,
                        PixelRect {
                            x,
                            y: row_range.start,
                            width,
                            height: row_range.end.saturating_sub(row_range.start),
                        },
                        &mut stats.allocation_growths,
                    );
                    index
                };
                push_reserved(&mut self.next_active, index, &mut stats.allocation_growths);
            }
            mem::swap(&mut self.active, &mut self.next_active);
        }
        &self.rects
    }
}

fn block_is_dirty(damage: DamageMap<'_>, grid: BlockGrid, block_x: usize, block_y: usize) -> bool {
    grid.block_index(block_x, block_y)
        .is_some_and(|index| damage.is_dirty(index))
}

fn encode_rects(
    frame: BgraFrame<'_>,
    rects: &[PixelRect],
    payload: &mut Vec<u8>,
    records: &mut Vec<PatchRecord>,
    stats: &mut ModelFrameStats,
) {
    for rect in rects {
        let offset = payload.len();
        append_rect(frame, *rect, payload, stats);
        let payload_len = payload.len().saturating_sub(offset);
        push_reserved(
            records,
            PatchRecord {
                rect: *rect,
                payload_offset: offset,
                payload_len,
            },
            &mut stats.allocation_growths,
        );
    }
}

fn append_rect(
    frame: BgraFrame<'_>,
    rect: PixelRect,
    payload: &mut Vec<u8>,
    stats: &mut ModelFrameStats,
) {
    let row_bytes = frame
        .grid()
        .width()
        .saturating_mul(crate::BGRA_BYTES_PER_PIXEL);
    if rect.x == 0 && rect.width == frame.grid().width() && frame.stride() == row_bytes {
        let start = rect.y.saturating_mul(frame.stride());
        let end = rect
            .y
            .saturating_add(rect.height)
            .saturating_mul(frame.stride());
        extend_reserved(
            payload,
            &frame.pixels()[start..end],
            &mut stats.allocation_growths,
        );
        note_copy(stats, end.saturating_sub(start), CopySide::Source);
        return;
    }

    let start_x = rect.x.saturating_mul(crate::BGRA_BYTES_PER_PIXEL);
    let width_bytes = rect.width.saturating_mul(crate::BGRA_BYTES_PER_PIXEL);
    for y in rect.y..rect.y.saturating_add(rect.height) {
        let row_start = y.saturating_mul(frame.stride()).saturating_add(start_x);
        let row_end = row_start.saturating_add(width_bytes);
        extend_reserved(
            payload,
            &frame.pixels()[row_start..row_end],
            &mut stats.allocation_growths,
        );
        note_copy(stats, width_bytes, CopySide::Source);
    }
}

fn apply_record(
    grid: BlockGrid,
    destination: &mut [u8],
    payload: &[u8],
    record: PatchRecord,
    stats: &mut ModelFrameStats,
) {
    let row_bytes = grid.width().saturating_mul(crate::BGRA_BYTES_PER_PIXEL);
    let rect_row_bytes = record
        .rect
        .width
        .saturating_mul(crate::BGRA_BYTES_PER_PIXEL);
    let source =
        &payload[record.payload_offset..record.payload_offset.saturating_add(record.payload_len)];
    if record.rect.x == 0 && record.rect.width == grid.width() {
        let destination_start = record.rect.y.saturating_mul(row_bytes);
        let destination_end = destination_start.saturating_add(source.len());
        destination[destination_start..destination_end].copy_from_slice(source);
        note_copy(stats, source.len(), CopySide::Compositor);
        return;
    }

    for (row, source_row) in source.chunks_exact(rect_row_bytes).enumerate() {
        let destination_start = record
            .rect
            .y
            .saturating_add(row)
            .saturating_mul(row_bytes)
            .saturating_add(record.rect.x.saturating_mul(crate::BGRA_BYTES_PER_PIXEL));
        let destination_end = destination_start.saturating_add(rect_row_bytes);
        destination[destination_start..destination_end].copy_from_slice(source_row);
        note_copy(stats, rect_row_bytes, CopySide::Compositor);
    }
}

fn copy_full_frame(
    frame: BgraFrame<'_>,
    destination: &mut [u8],
    stats: &mut ModelFrameStats,
    side: CopySide,
) {
    copy_full_width_rows(frame, destination, 0, frame.grid().height(), stats, side);
}

fn copy_full_width_rows(
    frame: BgraFrame<'_>,
    destination: &mut [u8],
    start_row: usize,
    end_row: usize,
    stats: &mut ModelFrameStats,
    side: CopySide,
) {
    let row_bytes = frame
        .grid()
        .width()
        .saturating_mul(crate::BGRA_BYTES_PER_PIXEL);
    if frame.stride() == row_bytes {
        let start = start_row.saturating_mul(row_bytes);
        let end = end_row.saturating_mul(row_bytes);
        destination[start..end].copy_from_slice(&frame.pixels()[start..end]);
        note_copy(stats, end.saturating_sub(start), side);
        return;
    }
    for row in start_row..end_row {
        let source_start = row.saturating_mul(frame.stride());
        let destination_start = row.saturating_mul(row_bytes);
        destination[destination_start..destination_start.saturating_add(row_bytes)]
            .copy_from_slice(&frame.pixels()[source_start..source_start.saturating_add(row_bytes)]);
        note_copy(stats, row_bytes, side);
    }
}

fn copy_rect_to_tight_frame(
    frame: BgraFrame<'_>,
    destination: &mut [u8],
    rect: PixelRect,
    stats: &mut ModelFrameStats,
    side: CopySide,
) {
    if rect.x == 0 && rect.width == frame.grid().width() {
        copy_full_width_rows(
            frame,
            destination,
            rect.y,
            rect.y.saturating_add(rect.height),
            stats,
            side,
        );
        return;
    }

    let destination_stride = frame
        .grid()
        .width()
        .saturating_mul(crate::BGRA_BYTES_PER_PIXEL);
    let x_bytes = rect.x.saturating_mul(crate::BGRA_BYTES_PER_PIXEL);
    let width_bytes = rect.width.saturating_mul(crate::BGRA_BYTES_PER_PIXEL);
    for row in rect.y..rect.y.saturating_add(rect.height) {
        let source_start = row.saturating_mul(frame.stride()).saturating_add(x_bytes);
        let destination_start = row
            .saturating_mul(destination_stride)
            .saturating_add(x_bytes);
        destination[destination_start..destination_start.saturating_add(width_bytes)]
            .copy_from_slice(
                &frame.pixels()[source_start..source_start.saturating_add(width_bytes)],
            );
        note_copy(stats, width_bytes, side);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CopySide {
    Source,
    Compositor,
}

fn note_copy(stats: &mut ModelFrameStats, bytes: usize, side: CopySide) {
    match side {
        CopySide::Source => {
            stats.source_copy_bytes = stats.source_copy_bytes.saturating_add(usize_to_u64(bytes));
            stats.source_copy_operations = stats.source_copy_operations.saturating_add(1);
        }
        CopySide::Compositor => {
            stats.compositor_copy_bytes = stats
                .compositor_copy_bytes
                .saturating_add(usize_to_u64(bytes));
            stats.compositor_copy_operations = stats.compositor_copy_operations.saturating_add(1);
        }
    }
}

fn push_reserved<T>(values: &mut Vec<T>, value: T, allocation_growths: &mut u64) {
    if values.len() == values.capacity() {
        *allocation_growths = allocation_growths.saturating_add(1);
    }
    values.push(value);
}

fn extend_reserved(values: &mut Vec<u8>, bytes: &[u8], allocation_growths: &mut u64) {
    if values.len().saturating_add(bytes.len()) > values.capacity() {
        *allocation_growths = allocation_growths.saturating_add(1);
    }
    values.extend_from_slice(bytes);
}
