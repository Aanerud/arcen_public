use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use arcen_media::audio::{
    AudioBitrateTier, OpusEncoder, ResolvedAudioStream, MAX_OPUS_PACKET_BYTES,
};
use arcen_protocol::messages::AudioStreamReason;
use arcen_protocol::AudioCodec;
use tokio::sync::Notify;

const AUDIO_QUEUE_CAPACITY: usize = 8;
const PCM_FRAME_BYTES: usize = 3_840;
const INTERLEAVED_SAMPLES: usize = 1_920;
const CODEC_FAILURE_THRESHOLD: u8 = 3;

pub struct AudioQueue {
    inner: Mutex<Inner>,
    notify: Notify,
    sent: AtomicU64,
    dropped: AtomicU64,
}

struct Inner {
    frames: VecDeque<EncodedAudioPacket>,
    closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAudioPacket {
    pub codec: AudioCodec,
    pub payload: Vec<u8>,
    pub timestamp_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeOutcome {
    Packet,
    Skipped,
    Disabled(AudioStreamReason),
}

pub struct AudioFrameEncoder {
    stream: ResolvedAudioStream,
    opus: Option<OpusEncoder>,
    pcm: [i16; INTERLEAVED_SAMPLES],
    packet: [u8; MAX_OPUS_PACKET_BYTES],
    consecutive_failures: u8,
}

impl AudioFrameEncoder {
    pub fn new(stream: ResolvedAudioStream) -> Result<Self, AudioStreamReason> {
        let opus = make_opus(stream)?;
        Ok(Self {
            stream,
            opus,
            pcm: [0; INTERLEAVED_SAMPLES],
            packet: [0; MAX_OPUS_PACKET_BYTES],
            consecutive_failures: 0,
        })
    }

    pub fn stream(&self) -> ResolvedAudioStream {
        self.stream
    }

    pub fn reconfigure(&mut self, stream: ResolvedAudioStream) -> Result<(), AudioStreamReason> {
        match (stream.codec, self.stream.codec) {
            (Some(AudioCodec::Opus), Some(AudioCodec::Opus)) => {
                if stream.bitrate != self.stream.bitrate {
                    self.opus
                        .as_mut()
                        .ok_or(AudioStreamReason::CodecUnavailable)?
                        .set_bitrate(stream.bitrate)
                        .map_err(|_| AudioStreamReason::CodecUnavailable)?;
                }
            }
            (Some(AudioCodec::Opus), _) => self.opus = Some(new_opus(stream.bitrate)?),
            _ => self.opus = None,
        }
        self.stream = stream;
        self.consecutive_failures = 0;
        Ok(())
    }

    pub fn reset_after_capture_restart(&mut self) -> Result<(), AudioStreamReason> {
        self.consecutive_failures = 0;
        if let Some(opus) = self.opus.as_mut() {
            opus.reset()
                .map_err(|_| AudioStreamReason::CodecUnavailable)?;
        }
        Ok(())
    }

    pub fn encode(
        &mut self,
        pcm_s16le: &[u8],
        timestamp_ms: u32,
        queue: &AudioQueue,
    ) -> EncodeOutcome {
        if !self.stream.is_enabled() {
            return EncodeOutcome::Skipped;
        }
        let packet = match self.stream.codec {
            Some(AudioCodec::Pcm) if pcm_s16le.len() == PCM_FRAME_BYTES => EncodedAudioPacket {
                codec: AudioCodec::Pcm,
                payload: pcm_s16le.to_vec(),
                timestamp_ms,
            },
            Some(AudioCodec::Opus) if pcm_s16le.len() == PCM_FRAME_BYTES => {
                for (sample, bytes) in self.pcm.iter_mut().zip(pcm_s16le.chunks_exact(2)) {
                    *sample = i16::from_le_bytes([bytes[0], bytes[1]]);
                }
                let encoded = self.opus.as_mut().ok_or(()).and_then(|encoder| {
                    encoder.encode(&self.pcm, &mut self.packet).map_err(|_| ())
                });
                match encoded {
                    Ok(encoded) => EncodedAudioPacket {
                        codec: AudioCodec::Opus,
                        payload: self.packet[..encoded].to_vec(),
                        timestamp_ms,
                    },
                    Err(()) => return self.codec_failure(),
                }
            }
            _ => return self.codec_failure(),
        };
        self.consecutive_failures = 0;
        queue.enqueue(packet);
        EncodeOutcome::Packet
    }

    fn codec_failure(&mut self) -> EncodeOutcome {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures < CODEC_FAILURE_THRESHOLD {
            return EncodeOutcome::Skipped;
        }
        self.consecutive_failures = 0;
        match make_opus(self.stream) {
            Ok(opus) => {
                self.opus = opus;
                EncodeOutcome::Skipped
            }
            Err(reason) => {
                self.opus = None;
                self.stream = ResolvedAudioStream::disabled(self.stream.mode, reason);
                EncodeOutcome::Disabled(AudioStreamReason::CodecFailure)
            }
        }
    }
}

fn make_opus(stream: ResolvedAudioStream) -> Result<Option<OpusEncoder>, AudioStreamReason> {
    match stream.codec {
        Some(AudioCodec::Opus) => new_opus(stream.bitrate).map(Some),
        _ => Ok(None),
    }
}

fn new_opus(bitrate: AudioBitrateTier) -> Result<OpusEncoder, AudioStreamReason> {
    OpusEncoder::new(bitrate).map_err(|_| AudioStreamReason::CodecUnavailable)
}

impl AudioQueue {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                frames: VecDeque::with_capacity(AUDIO_QUEUE_CAPACITY),
                closed: false,
            }),
            notify: Notify::new(),
            sent: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    pub fn enqueue(&self, frame: EncodedAudioPacket) {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return;
        }
        if inner.frames.len() == AUDIO_QUEUE_CAPACITY {
            inner.frames.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        inner.frames.push_back(frame);
        drop(inner);
        self.notify.notify_one();
    }

    pub async fn dequeue(&self) -> Option<EncodedAudioPacket> {
        loop {
            let notified = self.notify.notified();
            {
                let mut inner = self.inner.lock().unwrap();
                if let Some(frame) = inner.frames.pop_front() {
                    self.sent.fetch_add(1, Ordering::Relaxed);
                    return Some(frame);
                }
                if inner.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    pub fn clear(&self) {
        self.inner.lock().unwrap().frames.clear();
    }

    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        drop(inner);
        self.notify.notify_one();
    }

    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Default for AudioQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::audio::{AudioPolicy, AudioProtocolMode};

    fn pcm_packet(value: u8) -> EncodedAudioPacket {
        EncodedAudioPacket {
            codec: AudioCodec::Pcm,
            payload: vec![value],
            timestamp_ms: u32::from(value),
        }
    }

    #[tokio::test]
    async fn audio_queue_drops_oldest_and_stays_bounded() {
        let queue = AudioQueue::new();
        for value in 0..=AUDIO_QUEUE_CAPACITY {
            queue.enqueue(pcm_packet(value as u8));
        }
        assert_eq!(queue.dropped(), 1);
        assert_eq!(queue.dequeue().await, Some(pcm_packet(1)));
    }

    #[tokio::test]
    async fn audio_queue_drains_then_closes() {
        let queue = AudioQueue::new();
        queue.enqueue(pcm_packet(7));
        queue.close();
        assert_eq!(queue.dequeue().await, Some(pcm_packet(7)));
        assert_eq!(queue.dequeue().await, None);
    }

    #[test]
    fn encoding_happens_before_the_bounded_queue() {
        let policy = AudioPolicy {
            opus_available: true,
            pcm_available: true,
        };
        let stream = policy.resolve(Some(&policy.capabilities()), true, 128);
        let mut encoder = AudioFrameEncoder::new(stream).unwrap();
        let queue = AudioQueue::new();
        let pcm: Vec<u8> = (0..PCM_FRAME_BYTES)
            .map(|index| (index % 251) as u8)
            .collect();

        assert_eq!(
            encoder.encode(&pcm, 0x1122_3344, &queue),
            EncodeOutcome::Packet
        );
        let packet = queue.inner.lock().unwrap().frames.front().unwrap().clone();
        assert_eq!(packet.codec, AudioCodec::Opus);
        assert!((1..=MAX_OPUS_PACKET_BYTES).contains(&packet.payload.len()));
    }

    #[test]
    fn three_consecutive_failures_recreate_codec_state() {
        let policy = AudioPolicy {
            opus_available: true,
            pcm_available: true,
        };
        let stream = policy.resolve(Some(&policy.capabilities()), true, 128);
        let mut encoder = AudioFrameEncoder::new(stream).unwrap();
        let queue = AudioQueue::new();

        assert_eq!(encoder.encode(&[], 1, &queue), EncodeOutcome::Skipped);
        assert_eq!(encoder.encode(&[], 2, &queue), EncodeOutcome::Skipped);
        assert_eq!(encoder.encode(&[], 3, &queue), EncodeOutcome::Skipped);
        assert!(encoder.opus.is_some());
        assert_eq!(encoder.consecutive_failures, 0);
    }

    #[test]
    fn disabled_encoder_never_enqueues() {
        let stream = ResolvedAudioStream::disabled(
            AudioProtocolMode::V1,
            AudioStreamReason::DisabledByPolicy,
        );
        let mut encoder = AudioFrameEncoder::new(stream).unwrap();
        let queue = AudioQueue::new();
        assert_eq!(
            encoder.encode(&vec![0; PCM_FRAME_BYTES], 1, &queue),
            EncodeOutcome::Skipped
        );
        assert!(queue.inner.lock().unwrap().frames.is_empty());
        assert_eq!(
            stream.result(),
            Some(arcen_protocol::messages::AudioStreamResultMsg::disabled(
                AudioStreamReason::DisabledByPolicy
            ))
        );
    }
}
