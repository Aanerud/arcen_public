use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::protocol::{
    decode_audio_header, decode_video_header, AudioHeader, FrameType, ProtocolError, VideoHeader,
    AUDIO_HEADER_SIZE,
};

/// Four was too small for the 5-7 frame bursts QUIC delivers after congestion
/// window expansion at ~33ms RTT. Eight covers the observed worst-case burst
/// depth without inflating decode-queue latency at 60fps (8 × 16.7ms = 133ms
/// ceiling, well within a tolerable display latency budget).
pub const VIDEO_PACKET_LIMIT: usize = 8;
pub const VIDEO_BYTE_LIMIT: usize = 64 * 1024 * 1024;
pub const AUDIO_PACKET_LIMIT: usize = 8;
pub const AUDIO_BYTE_LIMIT: usize = 32 * 1024;

const MEDIA_BURST_GAP: Duration = Duration::from_millis(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaPacket {
    Video {
        header: VideoHeader,
        payload: Vec<u8>,
    },
    Audio {
        header: AudioHeader,
        payload: Vec<u8>,
    },
}

impl MediaPacket {
    pub fn timestamp_ms(&self) -> u32 {
        match self {
            Self::Video { header, .. } => header.timestamp_ms,
            Self::Audio { header, .. } => header.timestamp_ms,
        }
    }

    pub fn payload_len(&self) -> usize {
        match self {
            Self::Video { payload, .. } | Self::Audio { payload, .. } => payload.len(),
        }
    }

    pub fn payload(&self) -> &[u8] {
        match self {
            Self::Video { payload, .. } | Self::Audio { payload, .. } => payload,
        }
    }
}

pub fn parse_media_packet(data: &[u8]) -> Result<Option<MediaPacket>, ProtocolError> {
    let Some(frame_type) = data.first().copied() else {
        return Err(ProtocolError::ShortHeader);
    };
    match FrameType::try_from(frame_type)? {
        FrameType::Audio => {
            let header = decode_audio_header(data)?;
            Ok(Some(MediaPacket::Audio {
                header,
                payload: data.get(AUDIO_HEADER_SIZE..).unwrap_or_default().to_vec(),
            }))
        }
        FrameType::VideoFull
        | FrameType::VideoH264
        | FrameType::VideoH265
        | FrameType::VideoAv1
        | FrameType::RegionVideoFull
        | FrameType::RegionVideoH264
        | FrameType::RegionVideoH265
        | FrameType::RegionVideoAv1 => {
            let header = decode_video_header(data)?;
            Ok(Some(MediaPacket::Video {
                header,
                payload: data
                    .get(header.encoded_len()..)
                    .unwrap_or_default()
                    .to_vec(),
            }))
        }
        FrameType::VideoPartial => Ok(None),
        FrameType::Clipboard
        | FrameType::HidDeviceAdded
        | FrameType::HidDeviceRemoved
        | FrameType::HidReport
        | FrameType::UsbBridgeUrbSubmit
        | FrameType::UsbBridgeUrbCancel
        | FrameType::UsbBridgeUrbComplete
        | FrameType::AudioUpstream => Err(ProtocolError::UnknownFrameType(frame_type)),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IncomingMediaTelemetry {
    pub video_received: u64,
    pub audio_received: u64,
    pub video_dropped_packets: u64,
    pub video_dropped_bytes: u64,
    pub video_superseded_packets: u64,
    pub video_superseded_bytes: u64,
    pub audio_dropped_packets: u64,
    pub audio_dropped_bytes: u64,
    pub malformed_packets: u64,
    pub malformed_video_packets: u64,
    pub malformed_audio_packets: u64,
    pub video_loss_epochs: u64,
    pub video_queue_overflow_events: u64,
    pub video_overflow_last_oldest_age_ms: u64,
    pub video_overflow_max_oldest_age_ms: u64,
    pub video_overflow_last_burst_packets: usize,
    pub video_enqueue_burst_high_water: usize,
    pub audio_enqueue_burst_high_water: usize,
    pub video_depth: usize,
    pub video_bytes: usize,
    pub audio_depth: usize,
    pub audio_bytes: usize,
    pub video_high_water_depth: usize,
    pub video_high_water_bytes: usize,
    pub audio_high_water_depth: usize,
    pub audio_high_water_bytes: usize,
}

#[derive(Debug)]
pub struct IncomingMediaBatch {
    pub video: Vec<(VideoHeader, Vec<u8>)>,
    pub audio: Vec<(AudioHeader, Vec<u8>)>,
    pub video_discontinuity: bool,
    pub video_discontinuity_monitor_ids: Vec<u16>,
    pub idr_needed: bool,
    pub idr_needed_monitor_ids: Vec<u16>,
    pub malformed_error: Option<String>,
    pub telemetry: IncomingMediaTelemetry,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IncomingMediaEnqueue {
    pub accepted: bool,
    pub notify: bool,
}

#[derive(Clone)]
pub struct IncomingMediaSender {
    shared: Arc<SharedInbox>,
}

pub struct IncomingMediaReceiver {
    shared: Arc<SharedInbox>,
}

struct SharedInbox {
    inner: Mutex<InboxInner>,
}

struct InboxInner {
    limits: IncomingMediaLimits,
    video: VecDeque<(VideoHeader, Vec<u8>)>,
    audio: VecDeque<(AudioHeader, Vec<u8>)>,
    video_bytes: usize,
    audio_bytes: usize,
    telemetry: IncomingMediaTelemetry,
    seen_video_routes: BTreeSet<u16>,
    awaiting_keyframes: BTreeSet<u16>,
    pending_video_discontinuities: BTreeSet<u16>,
    pending_idr_routes: BTreeSet<u16>,
    malformed_error: Option<String>,
    telemetry_dirty: bool,
    notified: bool,
    video_first_enqueued_at: Option<Instant>,
    last_video_enqueued_at: Option<Instant>,
    current_video_burst_packets: usize,
    last_audio_enqueued_at: Option<Instant>,
    current_audio_burst_packets: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct IncomingMediaLimits {
    pub video_packets: usize,
    pub video_bytes: usize,
    pub audio_packets: usize,
    pub audio_bytes: usize,
}

impl IncomingMediaLimits {
    pub const fn new(
        video_packets: usize,
        video_bytes: usize,
        audio_packets: usize,
        audio_bytes: usize,
    ) -> Self {
        Self {
            video_packets,
            video_bytes,
            audio_packets,
            audio_bytes,
        }
    }
}

impl Default for IncomingMediaLimits {
    fn default() -> Self {
        Self::new(
            VIDEO_PACKET_LIMIT,
            VIDEO_BYTE_LIMIT,
            AUDIO_PACKET_LIMIT,
            AUDIO_BYTE_LIMIT,
        )
    }
}

pub fn incoming_media_inbox() -> (IncomingMediaSender, IncomingMediaReceiver) {
    incoming_media_inbox_with_limits(IncomingMediaLimits::default())
}

pub fn incoming_media_inbox_with_limits(
    limits: IncomingMediaLimits,
) -> (IncomingMediaSender, IncomingMediaReceiver) {
    let shared = Arc::new(SharedInbox {
        inner: Mutex::new(InboxInner {
            limits: IncomingMediaLimits {
                video_packets: limits.video_packets.max(1),
                video_bytes: limits.video_bytes.max(1),
                audio_packets: limits.audio_packets.max(1),
                audio_bytes: limits.audio_bytes.max(1),
            },
            video: VecDeque::with_capacity(limits.video_packets.max(1)),
            audio: VecDeque::with_capacity(limits.audio_packets.max(1)),
            video_bytes: 0,
            audio_bytes: 0,
            telemetry: IncomingMediaTelemetry::default(),
            seen_video_routes: BTreeSet::new(),
            awaiting_keyframes: BTreeSet::from([0]),
            pending_video_discontinuities: BTreeSet::new(),
            pending_idr_routes: BTreeSet::new(),
            malformed_error: None,
            telemetry_dirty: false,
            notified: false,
            video_first_enqueued_at: None,
            last_video_enqueued_at: None,
            current_video_burst_packets: 0,
            last_audio_enqueued_at: None,
            current_audio_burst_packets: 0,
        }),
    });
    (
        IncomingMediaSender {
            shared: Arc::clone(&shared),
        },
        IncomingMediaReceiver { shared },
    )
}

impl IncomingMediaSender {
    pub fn enqueue_bytes(&self, bytes: &[u8]) -> Result<IncomingMediaEnqueue, ProtocolError> {
        match parse_media_packet(bytes)? {
            Some(packet) => Ok(self.enqueue(packet)),
            None => Ok(IncomingMediaEnqueue::default()),
        }
    }

    pub fn enqueue(&self, packet: MediaPacket) -> IncomingMediaEnqueue {
        self.enqueue_at(packet, Instant::now())
    }

    fn enqueue_at(&self, packet: MediaPacket, now: Instant) -> IncomingMediaEnqueue {
        let mut inner = self.shared.inner.lock().expect("media inbox poisoned");
        let accepted = match packet {
            MediaPacket::Video { header, payload } => inner.enqueue_video(header, payload, now),
            MediaPacket::Audio { header, payload } => inner.enqueue_audio(header, payload, now),
        };
        let notify = !inner.notified && inner.has_work();
        if notify {
            inner.notified = true;
        }
        IncomingMediaEnqueue { accepted, notify }
    }

    pub fn snapshot(&self) -> IncomingMediaTelemetry {
        self.shared
            .inner
            .lock()
            .expect("media inbox poisoned")
            .current_telemetry()
    }

    pub fn record_malformed(&self, bytes: &[u8], error: String) -> IncomingMediaEnqueue {
        let mut inner = self.shared.inner.lock().expect("media inbox poisoned");
        inner.telemetry.malformed_packets += 1;
        inner.malformed_error = Some(error);
        inner.telemetry_dirty = true;
        if bytes.first().copied() == Some(FrameType::Audio as u8) {
            inner.telemetry.malformed_audio_packets += 1;
            inner.telemetry.audio_dropped_packets += 1;
            inner.telemetry.audio_dropped_bytes += bytes.len() as u64;
        } else {
            inner.telemetry.malformed_video_packets += 1;
            inner.telemetry.video_dropped_packets += inner.video.len() as u64 + 1;
            inner.telemetry.video_dropped_bytes += inner.video_bytes as u64 + bytes.len() as u64;
            inner.video.clear();
            inner.video_bytes = 0;
            inner.video_first_enqueued_at = None;
            let routes = if inner.seen_video_routes.is_empty() {
                vec![0]
            } else {
                inner.seen_video_routes.iter().copied().collect()
            };
            for monitor_id in routes {
                inner.mark_video_loss(monitor_id);
            }
        }
        let notify = !inner.notified;
        if notify {
            inner.notified = true;
        }
        IncomingMediaEnqueue {
            accepted: false,
            notify,
        }
    }
}

impl IncomingMediaReceiver {
    pub fn take_batch(&self) -> IncomingMediaBatch {
        let mut inner = self.shared.inner.lock().expect("media inbox poisoned");
        let video = inner.video.drain(..).collect();
        let audio = inner.audio.drain(..).collect();
        let video_discontinuity_monitor_ids = inner
            .pending_video_discontinuities
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let video_discontinuity = !video_discontinuity_monitor_ids.is_empty();
        inner.pending_video_discontinuities.clear();
        let idr_needed_monitor_ids = inner.pending_idr_routes.iter().copied().collect::<Vec<_>>();
        let idr_needed = !idr_needed_monitor_ids.is_empty();
        let malformed_error = inner.malformed_error.take();
        inner.video_bytes = 0;
        inner.audio_bytes = 0;
        inner.video_first_enqueued_at = None;
        inner.telemetry_dirty = false;
        inner.notified = false;
        IncomingMediaBatch {
            video,
            audio,
            video_discontinuity,
            video_discontinuity_monitor_ids,
            idr_needed,
            idr_needed_monitor_ids,
            malformed_error,
            telemetry: inner.current_telemetry(),
        }
    }

    pub fn snapshot(&self) -> IncomingMediaTelemetry {
        self.shared
            .inner
            .lock()
            .expect("media inbox poisoned")
            .current_telemetry()
    }
}

impl InboxInner {
    fn enqueue_video(&mut self, header: VideoHeader, payload: Vec<u8>, now: Instant) -> bool {
        self.telemetry.video_received += 1;
        self.observe_video_arrival(now);
        let monitor_id = header.monitor_id;
        let first_for_route = self.seen_video_routes.insert(monitor_id);
        if first_for_route {
            self.awaiting_keyframes.insert(monitor_id);
        }
        let packet_bytes = payload.len();
        let is_keyframe = header.is_keyframe();

        if !is_keyframe && self.awaiting_keyframes.contains(&monitor_id) {
            self.telemetry.video_dropped_packets += 1;
            self.telemetry.video_dropped_bytes += packet_bytes as u64;
            return false;
        }

        if packet_bytes > self.limits.video_bytes {
            let (cleared, cleared_bytes) = self.clear_video_route(monitor_id);
            self.telemetry.video_dropped_packets += cleared as u64 + 1;
            self.telemetry.video_dropped_bytes += cleared_bytes as u64 + packet_bytes as u64;
            self.mark_video_loss(monitor_id);
            return false;
        }

        if is_keyframe {
            let (cleared, cleared_bytes) = self.clear_video_route(monitor_id);
            self.telemetry.video_superseded_packets += cleared as u64;
            self.telemetry.video_superseded_bytes += cleared_bytes as u64;
            self.awaiting_keyframes.remove(&monitor_id);
            self.pending_idr_routes.remove(&monitor_id);
            if self.video.is_empty() {
                self.video_first_enqueued_at = Some(now);
            }
            self.video_bytes += packet_bytes;
            self.video.push_back((header, payload));
            self.update_high_water();
            return true;
        }

        let (route_packets, route_bytes) = self.video_route_usage(monitor_id);
        let exceeds_packets = route_packets >= self.limits.video_packets;
        let exceeds_bytes = route_bytes
            .checked_add(packet_bytes)
            .is_none_or(|bytes| bytes > self.limits.video_bytes);
        if exceeds_packets || exceeds_bytes {
            self.record_video_overflow(now);
            let (cleared, cleared_bytes) = self.clear_video_route(monitor_id);
            self.telemetry.video_dropped_packets += cleared as u64 + 1;
            self.telemetry.video_dropped_bytes += cleared_bytes as u64 + packet_bytes as u64;
            self.mark_video_loss(monitor_id);
            return false;
        }

        if self.video.is_empty() {
            self.video_first_enqueued_at = Some(now);
        }
        self.video_bytes += packet_bytes;
        self.video.push_back((header, payload));
        self.update_high_water();
        true
    }

    fn enqueue_audio(&mut self, header: AudioHeader, payload: Vec<u8>, now: Instant) -> bool {
        self.telemetry.audio_received += 1;
        self.observe_audio_arrival(now);
        let packet_bytes = payload.len();
        if packet_bytes > self.limits.audio_bytes {
            self.telemetry.audio_dropped_packets += 1;
            self.telemetry.audio_dropped_bytes += packet_bytes as u64;
            self.telemetry_dirty = true;
            return false;
        }
        while self.audio.len() >= self.limits.audio_packets
            || self
                .audio_bytes
                .checked_add(packet_bytes)
                .is_none_or(|bytes| bytes > self.limits.audio_bytes)
        {
            if let Some((_, dropped_payload)) = self.audio.pop_front() {
                self.audio_bytes = self.audio_bytes.saturating_sub(dropped_payload.len());
                self.telemetry.audio_dropped_packets += 1;
                self.telemetry.audio_dropped_bytes += dropped_payload.len() as u64;
                self.telemetry_dirty = true;
            } else {
                break;
            }
        }

        self.audio_bytes += packet_bytes;
        self.audio.push_back((header, payload));
        self.update_high_water();
        true
    }

    fn observe_video_arrival(&mut self, now: Instant) {
        self.current_video_burst_packets = if self
            .last_video_enqueued_at
            .is_some_and(|last| now.duration_since(last) <= MEDIA_BURST_GAP)
        {
            self.current_video_burst_packets.saturating_add(1)
        } else {
            1
        };
        self.last_video_enqueued_at = Some(now);
        self.telemetry.video_enqueue_burst_high_water = self
            .telemetry
            .video_enqueue_burst_high_water
            .max(self.current_video_burst_packets);
    }

    fn observe_audio_arrival(&mut self, now: Instant) {
        self.current_audio_burst_packets = if self
            .last_audio_enqueued_at
            .is_some_and(|last| now.duration_since(last) <= MEDIA_BURST_GAP)
        {
            self.current_audio_burst_packets.saturating_add(1)
        } else {
            1
        };
        self.last_audio_enqueued_at = Some(now);
        self.telemetry.audio_enqueue_burst_high_water = self
            .telemetry
            .audio_enqueue_burst_high_water
            .max(self.current_audio_burst_packets);
    }

    fn record_video_overflow(&mut self, now: Instant) {
        let oldest_age_ms = self.video_first_enqueued_at.map_or(0, |first| {
            u64::try_from(now.duration_since(first).as_millis()).unwrap_or(u64::MAX)
        });
        self.telemetry.video_queue_overflow_events =
            self.telemetry.video_queue_overflow_events.saturating_add(1);
        self.telemetry.video_overflow_last_oldest_age_ms = oldest_age_ms;
        self.telemetry.video_overflow_max_oldest_age_ms = self
            .telemetry
            .video_overflow_max_oldest_age_ms
            .max(oldest_age_ms);
        self.telemetry.video_overflow_last_burst_packets = self.current_video_burst_packets;
    }

    fn video_route_usage(&self, monitor_id: u16) -> (usize, usize) {
        self.video
            .iter()
            .filter(|(header, _)| header.monitor_id == monitor_id)
            .fold((0usize, 0usize), |(packets, bytes), (_, payload)| {
                (packets + 1, bytes.saturating_add(payload.len()))
            })
    }

    fn clear_video_route(&mut self, monitor_id: u16) -> (usize, usize) {
        let mut packets = 0usize;
        let mut bytes = 0usize;
        self.video.retain(|(header, payload)| {
            if header.monitor_id == monitor_id {
                packets += 1;
                bytes = bytes.saturating_add(payload.len());
                false
            } else {
                true
            }
        });
        self.video_bytes = self.video_bytes.saturating_sub(bytes);
        if self.video.is_empty() {
            self.video_first_enqueued_at = None;
        }
        (packets, bytes)
    }

    fn mark_video_loss(&mut self, monitor_id: u16) {
        self.telemetry.video_loss_epochs += 1;
        self.awaiting_keyframes.insert(monitor_id);
        self.pending_video_discontinuities.insert(monitor_id);
        self.pending_idr_routes.insert(monitor_id);
        self.telemetry_dirty = true;
    }

    fn update_high_water(&mut self) {
        self.telemetry.video_high_water_depth =
            self.telemetry.video_high_water_depth.max(self.video.len());
        self.telemetry.video_high_water_bytes =
            self.telemetry.video_high_water_bytes.max(self.video_bytes);
        self.telemetry.audio_high_water_depth =
            self.telemetry.audio_high_water_depth.max(self.audio.len());
        self.telemetry.audio_high_water_bytes =
            self.telemetry.audio_high_water_bytes.max(self.audio_bytes);
    }

    fn has_work(&self) -> bool {
        !self.video.is_empty()
            || !self.audio.is_empty()
            || !self.pending_video_discontinuities.is_empty()
            || self.telemetry_dirty
    }

    fn current_telemetry(&self) -> IncomingMediaTelemetry {
        IncomingMediaTelemetry {
            video_depth: self.video.len(),
            video_bytes: self.video_bytes,
            audio_depth: self.audio.len(),
            audio_bytes: self.audio_bytes,
            ..self.telemetry
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;
    use crate::protocol::{
        encode_audio_header, encode_video_header, AudioCodec, ChromaSubsampling, VideoCodec,
        VIDEO_KEYFRAME_FLAG,
    };

    fn limits(
        video_packets: usize,
        video_bytes: usize,
        audio_packets: usize,
    ) -> IncomingMediaLimits {
        IncomingMediaLimits::new(video_packets, video_bytes, audio_packets, 32)
    }

    fn video(timestamp_ms: u32, keyframe: bool, payload_len: usize) -> MediaPacket {
        video_for_monitor(0, timestamp_ms, keyframe, payload_len)
    }

    fn video_for_monitor(
        monitor_id: u16,
        timestamp_ms: u32,
        keyframe: bool,
        payload_len: usize,
    ) -> MediaPacket {
        MediaPacket::Video {
            header: VideoHeader {
                frame_type: if monitor_id == 0 {
                    FrameType::VideoH264
                } else {
                    FrameType::RegionVideoH264
                },
                codec: VideoCodec::H264,
                chroma: ChromaSubsampling::Yuv420,
                flags: u8::from(keyframe),
                timestamp_ms,
                monitor_id,
                topology_generation: u64::from(monitor_id != 0),
                stream_epoch: u64::from(monitor_id != 0),
            },
            payload: vec![timestamp_ms as u8; payload_len],
        }
    }

    fn audio(timestamp_ms: u32, payload_len: usize) -> MediaPacket {
        MediaPacket::Audio {
            header: AudioHeader {
                codec: AudioCodec::Pcm,
                timestamp_ms,
            },
            payload: vec![timestamp_ms as u8; payload_len],
        }
    }

    fn video_timestamps(batch: &IncomingMediaBatch) -> Vec<u32> {
        batch
            .video
            .iter()
            .map(|(header, _)| header.timestamp_ms)
            .collect()
    }

    #[test]
    fn parses_h264_video_packet() {
        let mut bytes = encode_video_header(VideoHeader {
            frame_type: FrameType::VideoH264,
            codec: VideoCodec::H264,
            chroma: ChromaSubsampling::Yuv444,
            flags: VIDEO_KEYFRAME_FLAG,
            timestamp_ms: 42,
            monitor_id: 0,
            topology_generation: 0,
            stream_epoch: 0,
        })
        .to_vec();
        bytes.extend_from_slice(&[1, 2, 3]);

        let packet = parse_media_packet(&bytes).unwrap().unwrap();
        match packet {
            MediaPacket::Video { header, payload } => {
                assert_eq!(header.timestamp_ms, 42);
                assert_eq!(payload, vec![1, 2, 3]);
            }
            other => panic!("unexpected packet: {other:?}"),
        }
    }

    #[test]
    fn rejects_legacy_region_video_before_queueing() {
        let bytes = encode_video_header(VideoHeader {
            frame_type: FrameType::VideoH264,
            codec: VideoCodec::H264,
            chroma: ChromaSubsampling::Yuv420,
            flags: VIDEO_KEYFRAME_FLAG,
            timestamp_ms: 42,
            monitor_id: 2,
            topology_generation: 0,
            stream_epoch: 0,
        });
        assert_eq!(
            parse_media_packet(&bytes),
            Err(ProtocolError::LegacyVideoMonitorId)
        );
    }

    #[test]
    fn region_video_payload_starts_after_the_v1_header() {
        let mut bytes = encode_video_header(VideoHeader {
            frame_type: FrameType::RegionVideoH265,
            codec: VideoCodec::H265,
            chroma: ChromaSubsampling::Yuv420,
            flags: VIDEO_KEYFRAME_FLAG,
            timestamp_ms: 42,
            monitor_id: 2,
            topology_generation: 7,
            stream_epoch: 11,
        });
        bytes.extend_from_slice(&[0xAA, 0xBB]);

        let packet = parse_media_packet(&bytes).unwrap().unwrap();
        match packet {
            MediaPacket::Video { header, payload } => {
                assert_eq!(header.topology_generation, 7);
                assert_eq!(header.stream_epoch, 11);
                assert_eq!(payload, vec![0xAA, 0xBB]);
            }
            other => panic!("unexpected packet: {other:?}"),
        }
    }

    #[test]
    fn parses_audio_packet() {
        let mut bytes = encode_audio_header(AudioHeader {
            codec: AudioCodec::Opus,
            timestamp_ms: 7,
        })
        .to_vec();
        bytes.extend_from_slice(&[9, 8]);

        let packet = parse_media_packet(&bytes).unwrap().unwrap();
        assert_eq!(packet.timestamp_ms(), 7);
        assert_eq!(packet.payload_len(), 2);
    }

    #[test]
    fn under_limit_video_drains_fifo() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(4, 16, 2));
        assert!(tx.enqueue(video(1, true, 1)).notify);
        assert!(!tx.enqueue(video(2, false, 1)).notify);
        assert!(!tx.enqueue(video(3, false, 1)).notify);

        let batch = rx.take_batch();
        assert_eq!(video_timestamps(&batch), vec![1, 2, 3]);
        assert!(!batch.video_discontinuity);
        assert!(!batch.idr_needed);
    }

    #[test]
    fn video_overflow_clears_full_chain_and_latches_one_idr() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(2, 8, 2));
        assert!(tx.enqueue(video(1, true, 1)).notify);
        assert!(!tx.enqueue(video(2, false, 2)).notify);
        assert!(!tx.enqueue(video(3, false, 3)).notify);
        assert!(!tx.enqueue(video(4, false, 1)).notify);

        let batch = rx.take_batch();
        assert!(batch.video_discontinuity);
        assert_eq!(batch.video_discontinuity_monitor_ids, vec![0]);
        assert!(batch.idr_needed);
        assert_eq!(batch.idr_needed_monitor_ids, vec![0]);
        assert!(batch.video.is_empty());
        assert_eq!(batch.telemetry.video_dropped_packets, 4);
        assert_eq!(batch.telemetry.video_loss_epochs, 1);
        assert!(!tx.enqueue(video(5, false, 1)).notify);
        assert!(rx.take_batch().idr_needed);
        tx.enqueue(video(10, true, 1));
        assert!(!rx.take_batch().idr_needed);
    }

    #[test]
    fn keyframe_supersedes_stale_prediction_chain() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(4, 16, 2));
        tx.enqueue(video(1, true, 1));
        tx.enqueue(video(2, false, 2));
        tx.enqueue(video(3, true, 3));
        tx.enqueue(video(4, false, 1));

        let batch = rx.take_batch();
        assert_eq!(video_timestamps(&batch), vec![3, 4]);
        assert!(!batch.idr_needed);
        assert_eq!(batch.telemetry.video_superseded_packets, 2);
        assert_eq!(batch.telemetry.video_superseded_bytes, 3);
    }

    #[test]
    fn one_monitors_keyframe_never_supersedes_another_monitors_chain() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(4, 16, 2));
        tx.enqueue(video_for_monitor(1, 1, true, 1));
        tx.enqueue(video_for_monitor(1, 2, false, 1));
        tx.enqueue(video_for_monitor(2, 10, true, 1));
        tx.enqueue(video_for_monitor(2, 11, false, 1));

        let batch = rx.take_batch();
        assert_eq!(
            batch
                .video
                .iter()
                .map(|(header, _)| (header.monitor_id, header.timestamp_ms))
                .collect::<Vec<_>>(),
            vec![(1, 1), (1, 2), (2, 10), (2, 11)]
        );
        assert!(!batch.video_discontinuity);
        assert!(!batch.idr_needed);
    }

    #[test]
    fn overflow_is_fenced_to_only_the_affected_monitor() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(1, 8, 2));
        tx.enqueue(video_for_monitor(1, 1, true, 1));
        tx.enqueue(video_for_monitor(2, 10, true, 1));
        tx.enqueue(video_for_monitor(1, 2, false, 1));

        let batch = rx.take_batch();
        assert_eq!(
            batch
                .video
                .iter()
                .map(|(header, _)| (header.monitor_id, header.timestamp_ms))
                .collect::<Vec<_>>(),
            vec![(2, 10)]
        );
        assert!(batch.video_discontinuity);
        assert_eq!(batch.video_discontinuity_monitor_ids, vec![1]);
        assert!(batch.idr_needed);
        assert_eq!(batch.idr_needed_monitor_ids, vec![1]);

        tx.enqueue(video_for_monitor(1, 3, true, 1));
        let recovered = rx.take_batch();
        assert_eq!(recovered.video[0].0.monitor_id, 1);
        assert!(!recovered.idr_needed);
    }

    #[test]
    fn replacement_keyframe_resumes_without_pre_loss_survivors() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(2, 8, 2));
        tx.enqueue(video(1, true, 1));
        tx.enqueue(video(2, false, 1));
        tx.enqueue(video(3, false, 1));
        assert!(rx.take_batch().video.is_empty());

        assert!(tx.enqueue(video(10, true, 1)).notify);
        tx.enqueue(video(11, false, 1));
        assert_eq!(video_timestamps(&rx.take_batch()), vec![10, 11]);
    }

    #[test]
    fn replacement_before_loss_observation_suppresses_pending_idr() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(1, 8, 2));
        tx.enqueue(video(1, true, 1));
        tx.enqueue(video(2, false, 1));
        tx.enqueue(video(10, true, 1));

        let batch = rx.take_batch();
        assert!(batch.video_discontinuity);
        assert!(!batch.idr_needed);
        assert_eq!(video_timestamps(&batch), vec![10]);
        assert_eq!(batch.telemetry.video_loss_epochs, 1);
    }

    #[test]
    fn waiting_p_frames_never_relatch_recovery_even_when_oversized() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(1, 8, 2));
        tx.enqueue(video(1, true, 1));
        tx.enqueue(video(2, false, 1));
        let loss = rx.take_batch();
        assert!(loss.idr_needed);
        assert_eq!(loss.telemetry.video_loss_epochs, 1);

        let outcome = tx.enqueue(video(3, false, 9));
        assert!(!outcome.accepted);
        assert!(!outcome.notify);
        assert_eq!(rx.snapshot().video_loss_epochs, 1);
        assert!(rx.take_batch().idr_needed);
    }

    #[test]
    fn oversized_keyframe_drops_the_queued_chain_instead_of_superseding_it() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(4, 5, 2));
        tx.enqueue(video(1, true, 2));
        tx.enqueue(video(2, false, 2));
        tx.enqueue(video(10, true, 6));

        let batch = rx.take_batch();
        assert!(batch.video.is_empty());
        assert!(batch.video_discontinuity);
        assert!(batch.idr_needed);
        assert_eq!(batch.telemetry.video_dropped_packets, 3);
        assert_eq!(batch.telemetry.video_dropped_bytes, 10);
        assert_eq!(batch.telemetry.video_superseded_packets, 0);
    }

    #[test]
    fn byte_overflow_clears_chain_and_default_admits_50_mib_idr() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(4, 5, 2));
        tx.enqueue(video(1, true, 4));
        tx.enqueue(video(2, false, 2));
        let batch = rx.take_batch();
        assert!(batch.video.is_empty());
        assert!(batch.video_discontinuity);
        assert_eq!(batch.telemetry.video_dropped_bytes, 6);

        let (tx, rx) = incoming_media_inbox();
        let payload_len = 50 * 1024 * 1024;
        assert!(tx.enqueue(video(10, true, payload_len)).accepted);
        assert_eq!(tx.snapshot().video_bytes, payload_len);
        assert_eq!(rx.take_batch().video.len(), 1);
    }

    #[test]
    fn audio_overflow_drops_oldest_to_keep_fresh_audio() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(2, 8, 2));
        tx.enqueue(audio(1, 4));
        tx.enqueue(audio(2, 4));
        assert!(tx.enqueue(audio(3, 4)).accepted);

        let batch = rx.take_batch();
        assert_eq!(
            batch
                .audio
                .iter()
                .map(|(header, _)| header.timestamp_ms)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(batch.telemetry.audio_dropped_packets, 1);
        assert_eq!(batch.telemetry.audio_dropped_bytes, 4);
    }

    #[test]
    fn video_and_audio_limits_are_independent() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(1, 8, 1));
        tx.enqueue(video(1, true, 1));
        tx.enqueue(audio(1, 1));
        tx.enqueue(video(2, false, 2));

        let batch = rx.take_batch();
        assert!(batch.video.is_empty());
        assert_eq!(batch.audio.len(), 1);
        assert_eq!(batch.telemetry.video_dropped_packets, 2);
        assert_eq!(batch.telemetry.audio_dropped_packets, 0);
    }

    #[test]
    fn readiness_is_coalesced_and_drain_enqueue_race_loses_no_wake() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(2, 8, 2));
        assert!(tx.enqueue(video(1, true, 1)).notify);
        assert!(!tx.enqueue(audio(1, 1)).notify);
        let _ = rx.take_batch();
        assert!(tx.enqueue(audio(2, 1)).notify);
        let _ = rx.take_batch();

        for _ in 0..100 {
            let (tx, rx) = incoming_media_inbox_with_limits(limits(2, 8, 2));
            assert!(tx.enqueue(video(1, true, 1)).notify);
            let barrier = Arc::new(Barrier::new(2));
            let producer = {
                let tx = tx.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    tx.enqueue(video(2, false, 1))
                })
            };
            barrier.wait();
            let first = rx.take_batch();
            let outcome = producer.join().unwrap();
            assert!(
                video_timestamps(&first).contains(&2) || outcome.notify,
                "racing packet was neither drained nor accompanied by a wake"
            );
            if outcome.notify {
                assert_eq!(video_timestamps(&rx.take_batch()), vec![2]);
            }
        }
    }

    #[test]
    fn oversized_video_is_rejected_and_wakes_recovery() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(2, 2, 2));
        let MediaPacket::Video { header, .. } = video(1, true, 1) else {
            unreachable!()
        };
        let outcome = tx.enqueue(MediaPacket::Video {
            header,
            payload: vec![0; 3],
        });
        assert!(outcome.notify);

        let batch = rx.take_batch();
        assert!(batch.video.is_empty());
        assert!(batch.video_discontinuity);
        assert!(batch.idr_needed);
        assert_eq!(batch.telemetry.video_dropped_packets, 1);
    }

    #[test]
    fn telemetry_counters_and_high_water_are_exact() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(2, 10, 1));
        tx.enqueue(video(1, true, 2));
        tx.enqueue(video(2, false, 3));
        tx.enqueue(video(10, true, 4));
        tx.enqueue(audio(1, 3));
        tx.enqueue(audio(2, 1));

        let telemetry = rx.take_batch().telemetry;
        assert_eq!(telemetry.video_received, 3);
        assert_eq!(telemetry.video_superseded_packets, 2);
        assert_eq!(telemetry.video_superseded_bytes, 5);
        assert_eq!(telemetry.video_dropped_packets, 0);
        assert_eq!(telemetry.video_high_water_depth, 2);
        assert_eq!(telemetry.video_high_water_bytes, 5);
        assert_eq!(telemetry.audio_received, 2);
        assert_eq!(telemetry.audio_dropped_packets, 1);
        assert_eq!(telemetry.audio_dropped_bytes, 3);
        assert_eq!(telemetry.audio_high_water_depth, 1);
        assert_eq!(telemetry.audio_high_water_bytes, 3);
        assert!(telemetry.video_enqueue_burst_high_water >= 1);
        assert!(telemetry.audio_enqueue_burst_high_water >= 1);
        assert_eq!(telemetry.video_depth, 0);
        assert_eq!(telemetry.video_bytes, 0);
        assert_eq!(telemetry.audio_depth, 0);
        assert_eq!(telemetry.audio_bytes, 0);
    }

    #[test]
    fn overflow_telemetry_distinguishes_burst_from_slow_consumer_pressure() {
        let start = Instant::now();
        let (tx, rx) = incoming_media_inbox_with_limits(limits(2, 8, 2));
        tx.enqueue_at(video(1, true, 1), start);
        tx.enqueue_at(video(2, false, 1), start + Duration::from_millis(1));
        tx.enqueue_at(video(3, false, 1), start + Duration::from_millis(2));

        let burst = rx.take_batch().telemetry;
        assert_eq!(burst.video_queue_overflow_events, 1);
        assert_eq!(burst.video_overflow_last_oldest_age_ms, 2);
        assert_eq!(burst.video_overflow_last_burst_packets, 3);
        assert_eq!(burst.video_enqueue_burst_high_water, 3);

        let (tx, rx) = incoming_media_inbox_with_limits(limits(2, 8, 2));
        tx.enqueue_at(video(1, true, 1), start);
        tx.enqueue_at(video(2, false, 1), start + Duration::from_millis(20));
        tx.enqueue_at(video(3, false, 1), start + Duration::from_millis(40));

        let slow = rx.take_batch().telemetry;
        assert_eq!(slow.video_queue_overflow_events, 1);
        assert_eq!(slow.video_overflow_last_oldest_age_ms, 40);
        assert_eq!(slow.video_overflow_last_burst_packets, 1);
        assert_eq!(slow.video_enqueue_burst_high_water, 1);
    }

    #[test]
    fn malformed_media_telemetry_is_coalesced_through_the_bounded_inbox() {
        let (tx, rx) = incoming_media_inbox();
        assert!(
            tx.record_malformed(&[FrameType::Audio as u8], "first".to_string())
                .notify
        );
        assert!(
            !tx.record_malformed(&[FrameType::Audio as u8], "latest".to_string())
                .notify
        );

        let batch = rx.take_batch();
        assert_eq!(batch.telemetry.malformed_packets, 2);
        assert_eq!(batch.telemetry.malformed_audio_packets, 2);
        assert_eq!(batch.telemetry.malformed_video_packets, 0);
        assert_eq!(batch.malformed_error.as_deref(), Some("latest"));
        assert!(!rx.take_batch().idr_needed);
    }

    #[test]
    fn malformed_audio_drops_only_audio_and_preserves_video_chain() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(4, 16, 2));
        tx.enqueue(video(1, true, 1));
        tx.enqueue(video(2, false, 1));

        let bytes = [FrameType::Audio as u8];
        assert_eq!(
            tx.enqueue_bytes(&bytes).unwrap_err(),
            ProtocolError::ShortHeader
        );
        tx.record_malformed(&bytes, "short audio header".to_string());

        let batch = rx.take_batch();
        assert_eq!(video_timestamps(&batch), vec![1, 2]);
        assert!(!batch.video_discontinuity);
        assert!(!batch.idr_needed);
        assert_eq!(batch.telemetry.malformed_audio_packets, 1);
        assert_eq!(batch.telemetry.malformed_video_packets, 0);
        assert_eq!(batch.telemetry.audio_dropped_packets, 1);
    }

    #[test]
    fn malformed_video_invalidates_chain_and_suppresses_later_p_frames() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(4, 16, 2));
        tx.enqueue(video(1, true, 1));
        tx.enqueue(video(2, false, 1));

        let bytes = [FrameType::VideoH264 as u8];
        assert_eq!(
            tx.enqueue_bytes(&bytes).unwrap_err(),
            ProtocolError::ShortHeader
        );
        tx.record_malformed(&bytes, "short video header".to_string());
        assert!(!tx.enqueue(video(3, false, 1)).accepted);

        let batch = rx.take_batch();
        assert!(batch.video.is_empty());
        assert!(batch.video_discontinuity);
        assert!(batch.idr_needed);
        assert_eq!(batch.telemetry.malformed_video_packets, 1);
        assert_eq!(batch.telemetry.video_loss_epochs, 1);
        assert_eq!(batch.telemetry.video_dropped_packets, 4);
    }

    #[test]
    fn unknown_binary_invalidates_video_until_keyframe() {
        let (tx, rx) = incoming_media_inbox_with_limits(limits(4, 16, 2));
        tx.enqueue(video(1, true, 1));
        let bytes = [0xff, 1, 2];
        assert_eq!(
            tx.enqueue_bytes(&bytes).unwrap_err(),
            ProtocolError::UnknownFrameType(0xff)
        );
        tx.record_malformed(&bytes, "unknown frame type".to_string());
        assert!(!tx.enqueue(video(2, false, 1)).accepted);
        assert!(rx.take_batch().idr_needed);

        tx.enqueue(video(10, true, 1));
        let recovery = rx.take_batch();
        assert_eq!(video_timestamps(&recovery), vec![10]);
        assert!(!recovery.idr_needed);
    }

    #[test]
    fn slow_consumer_soak_never_exceeds_lane_bounds() {
        let (tx, rx) = incoming_media_inbox();
        for sequence in 0..50_000_u32 {
            tx.enqueue(video(sequence, sequence % 120 == 0, 1024));
            tx.enqueue(audio(sequence, 1024));
        }

        let snapshot = rx.snapshot();
        assert!(snapshot.video_depth <= VIDEO_PACKET_LIMIT);
        assert!(snapshot.video_bytes <= VIDEO_BYTE_LIMIT);
        assert!(snapshot.audio_depth <= AUDIO_PACKET_LIMIT);
        assert!(snapshot.audio_bytes <= AUDIO_BYTE_LIMIT);
        assert!(snapshot.video_loss_epochs > 0);
        assert!(snapshot.audio_dropped_packets > 0);
    }
}
