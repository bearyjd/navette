use std::collections::{HashMap, VecDeque};

use navette_protocol::media::{MediaKind, MediaPacket, StreamConfig};

use crate::decoder::{DecodedFrame, Decoder, DecoderConfig, DecoderError};

/// Builds a decoder for a newly configured stream. The real viewer hands back
/// an `FfmpegDecoder`; tests hand back a `FakeDecoder`.
pub type DecoderFactory =
    Box<dyn FnMut(&DecoderConfig) -> Result<Box<dyn Decoder>, DecoderError> + Send>;

/// A decoded picture together with the surface identity it belongs to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamFrame {
    pub stream_id: u64,
    pub client_id: u64,
    pub surface_id: u64,
    pub timestamp_us: u64,
    pub frame: DecodedFrame,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamEvent {
    Frame(StreamFrame),
    /// The toplevel closed; the stream's decoder has been torn down.
    Ended {
        stream_id: u64,
    },
    /// An access unit failed to decode. The connection stays up and the
    /// bridge is asked for a fresh keyframe.
    DecodeFailed {
        stream_id: u64,
    },
}

struct StreamDecoder {
    client_id: u64,
    surface_id: u64,
    decoder: Box<dyn Decoder>,
    /// Timestamps of access units submitted but not yet paired with a
    /// decoded frame, oldest first. The decode pipeline runs a few access
    /// units behind, so the frame `decode` returns is never the one just
    /// submitted — it is whichever access unit is at the front of this
    /// queue.
    pending_timestamps: VecDeque<u64>,
}

/// Routes the packets of one media session to a decoder per `stream_id`.
///
/// This is deliberately synchronous so the protocol behaviour it owns —
/// bootstrapping, reconfiguration, teardown and malformed ordering — can be
/// exercised against fixture packets without a socket.
pub struct StreamRouter {
    streams: HashMap<u64, StreamDecoder>,
    factory: DecoderFactory,
}

impl StreamRouter {
    pub fn new(factory: DecoderFactory) -> Self {
        Self {
            streams: HashMap::new(),
            factory,
        }
    }

    /// Number of streams with a live decoder.
    pub fn live_streams(&self) -> usize {
        self.streams.len()
    }

    /// Feeds one packet through the router. Zero or more events come back: a
    /// packet can bootstrap a stream (no event), produce zero, one, or
    /// several frames (the decode pipeline runs a few access units behind
    /// and its output is fully drained on every call), end a stream (a
    /// flush of any buffered frames followed by `Ended`), or fail to decode
    /// (`DecodeFailed`).
    pub fn handle(&mut self, packet: &MediaPacket) -> Vec<StreamEvent> {
        match packet.header.kind {
            MediaKind::StreamConfig => {
                self.configure(packet);
                Vec::new()
            }
            MediaKind::Video => self.decode(packet),
            MediaKind::StreamEnd => self.end(packet.header.stream_id),
            MediaKind::Metrics => Vec::new(),
        }
    }

    fn configure(&mut self, packet: &MediaPacket) {
        let stream_id = packet.header.stream_id;
        let config = match StreamConfig::decode(&packet.payload) {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(stream_id, %error, "discarding malformed stream configuration");
                return;
            }
        };
        let decoder_config = DecoderConfig {
            width: packet.header.width,
            height: packet.header.height,
            codec_config: config.codec_config,
        };

        if let Some(stream) = self.streams.get_mut(&stream_id) {
            if stream.client_id == config.client_id
                && stream.surface_id == config.surface_id
                && stream.decoder.config() == &decoder_config
            {
                // The hub replays the latest configuration on every attach.
                tracing::trace!(stream_id, "stream configuration is unchanged");
                return;
            }
            if let Err(error) = stream.decoder.reconfigure(decoder_config) {
                tracing::error!(stream_id, %error, "failed to reconfigure decoder");
                self.streams.remove(&stream_id);
                return;
            }
            stream.client_id = config.client_id;
            stream.surface_id = config.surface_id;
            // The rebuilt decoder's output no longer corresponds to any
            // access unit submitted before the reconfigure.
            stream.pending_timestamps.clear();
            tracing::debug!(stream_id, "decoder reconfigured");
            return;
        }

        match (self.factory)(&decoder_config) {
            Ok(decoder) => {
                tracing::debug!(
                    stream_id,
                    client_id = config.client_id,
                    surface_id = config.surface_id,
                    "decoder created"
                );
                self.streams.insert(
                    stream_id,
                    StreamDecoder {
                        client_id: config.client_id,
                        surface_id: config.surface_id,
                        decoder,
                        pending_timestamps: VecDeque::new(),
                    },
                );
            }
            Err(error) => tracing::error!(stream_id, %error, "failed to create decoder"),
        }
    }

    fn decode(&mut self, packet: &MediaPacket) -> Vec<StreamEvent> {
        let stream_id = packet.header.stream_id;
        if packet.payload.is_empty() {
            // Malformed rather than fatal: an empty access unit says nothing
            // about the decoder, so it is dropped like any other bad input.
            tracing::warn!(stream_id, "dropping video packet with an empty payload");
            return Vec::new();
        }
        let Some(stream) = self.streams.get_mut(&stream_id) else {
            tracing::warn!(stream_id, "dropping video for an unconfigured stream");
            return Vec::new();
        };
        stream
            .pending_timestamps
            .push_back(packet.header.timestamp_us);
        match stream.decoder.decode(&packet.payload) {
            Ok(frames) => frames
                .into_iter()
                .map(|frame| {
                    // Each drained frame is paired with the oldest still
                    // outstanding access unit, not the one just submitted —
                    // the pipeline is a few access units behind.
                    let timestamp_us = stream
                        .pending_timestamps
                        .pop_front()
                        .unwrap_or(packet.header.timestamp_us);
                    StreamEvent::Frame(StreamFrame {
                        stream_id,
                        client_id: stream.client_id,
                        surface_id: stream.surface_id,
                        timestamp_us,
                        frame,
                    })
                })
                .collect(),
            Err(error) => {
                tracing::warn!(stream_id, %error, "decode failed; requesting a keyframe");
                // A decode error leaves this stream's decoder in an unknown
                // state — for `FfmpegDecoder` every reachable variant means
                // its subprocess is gone, or it has stalled without ever
                // recovering — so the decoder is discarded rather than left
                // to fail (or spin silently) on every later frame. The
                // keyframe request the client sends next is expected to
                // bring a fresh `stream_config` that rebuilds it. Malformed
                // input never reaches here: it is dropped above.
                self.streams.remove(&stream_id);
                vec![StreamEvent::DecodeFailed { stream_id }]
            }
        }
    }

    fn end(&mut self, stream_id: u64) -> Vec<StreamEvent> {
        let Some(mut stream) = self.streams.remove(&stream_id) else {
            tracing::debug!(stream_id, "ignoring end of an unconfigured stream");
            return Vec::new();
        };
        // Flush whatever the pipeline had already decoded but this router
        // never retrieved — otherwise the last pictures of a closing window
        // vanish silently instead of reaching the caller.
        let mut events: Vec<StreamEvent> = stream
            .decoder
            .drain()
            .into_iter()
            .map(|frame| {
                let timestamp_us = stream.pending_timestamps.pop_front().unwrap_or(0);
                StreamEvent::Frame(StreamFrame {
                    stream_id,
                    client_id: stream.client_id,
                    surface_id: stream.surface_id,
                    timestamp_us,
                    frame,
                })
            })
            .collect();
        tracing::debug!(stream_id, "stream ended");
        events.push(StreamEvent::Ended { stream_id });
        events
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use navette_protocol::media::{MediaFlags, MediaHeader};

    use super::*;
    use crate::decoder::{DecoderMetrics, FakeDecoder};

    const CODEC_CONFIG: [u8; 5] = [0, 0, 0, 1, 0x67];
    const OTHER_CODEC_CONFIG: [u8; 6] = [0, 0, 0, 1, 0x67, 2];

    fn packet(kind: MediaKind, stream_id: u64, sequence: u64, payload: Vec<u8>) -> MediaPacket {
        MediaPacket::new(
            MediaHeader {
                kind,
                flags: MediaFlags::new(kind == MediaKind::Video, false),
                stream_id,
                sequence,
                timestamp_us: sequence * 1000,
                payload_len: 0,
                width: 4,
                height: 2,
            },
            payload,
        )
        .unwrap()
    }

    fn stream_config(
        stream_id: u64,
        sequence: u64,
        client_id: u64,
        surface_id: u64,
        codec_config: &[u8],
    ) -> MediaPacket {
        stream_config_sized(
            stream_id,
            sequence,
            client_id,
            surface_id,
            codec_config,
            4,
            2,
        )
    }

    /// Like [`stream_config`] but with caller-chosen coded dimensions, so a
    /// resize can be exercised without changing the codec bootstrap.
    fn stream_config_sized(
        stream_id: u64,
        sequence: u64,
        client_id: u64,
        surface_id: u64,
        codec_config: &[u8],
        width: u32,
        height: u32,
    ) -> MediaPacket {
        let payload = StreamConfig {
            client_id,
            surface_id,
            codec_config: codec_config.to_vec(),
        }
        .encode()
        .unwrap();
        MediaPacket::new(
            MediaHeader {
                kind: MediaKind::StreamConfig,
                flags: MediaFlags::new(false, false),
                stream_id,
                sequence,
                timestamp_us: sequence * 1000,
                payload_len: 0,
                width,
                height,
            },
            payload,
        )
        .unwrap()
    }

    fn video(stream_id: u64, sequence: u64) -> MediaPacket {
        packet(
            MediaKind::Video,
            stream_id,
            sequence,
            vec![0, 0, 0, 1, 0x65, sequence as u8],
        )
    }

    fn fake_router() -> StreamRouter {
        StreamRouter::new(Box::new(|config: &DecoderConfig| {
            Ok(Box::new(FakeDecoder::new(config.clone())?) as Box<dyn Decoder>)
        }))
    }

    /// Expects exactly one decoded-frame event and unwraps it. `FakeDecoder`
    /// always yields exactly one frame per access unit, so every test below
    /// that drives it can rely on `handle` returning a single-element vec.
    fn frame_of(mut events: Vec<StreamEvent>) -> StreamFrame {
        assert_eq!(
            events.len(),
            1,
            "expected exactly one event, got {events:?}"
        );
        match events.pop() {
            Some(StreamEvent::Frame(frame)) => frame,
            other => panic!("expected a decoded frame, got {other:?}"),
        }
    }

    #[test]
    fn stream_config_bootstraps_decoding_and_carries_surface_identity() {
        let mut router = fake_router();
        assert!(
            router
                .handle(&stream_config(7, 1, 11, 12, &CODEC_CONFIG))
                .is_empty()
        );
        assert_eq!(router.live_streams(), 1);

        let frame = frame_of(router.handle(&video(7, 2)));
        assert_eq!(frame.stream_id, 7);
        assert_eq!(frame.client_id, 11);
        assert_eq!(frame.surface_id, 12);
        assert_eq!(frame.timestamp_us, 2000);
        assert_eq!((frame.frame.width, frame.frame.height), (4, 2));
        assert_eq!(frame.frame.pixels, vec![0; 4 * 2 * 4]);
    }

    #[test]
    fn video_without_configuration_is_dropped_without_decoding() {
        let mut router = fake_router();
        assert!(router.handle(&video(7, 1)).is_empty());
        assert_eq!(router.live_streams(), 0);

        // The stream still bootstraps cleanly afterwards, and its decoder
        // starts from scratch rather than having consumed the dropped packet.
        router.handle(&stream_config(7, 2, 11, 12, &CODEC_CONFIG));
        assert_eq!(frame_of(router.handle(&video(7, 3))).frame.pixels[0], 0);
    }

    #[test]
    fn concurrent_streams_keep_independent_decoder_state() {
        let mut router = fake_router();
        router.handle(&stream_config(1, 1, 11, 12, &CODEC_CONFIG));
        router.handle(&stream_config(2, 1, 21, 22, &CODEC_CONFIG));

        assert_eq!(frame_of(router.handle(&video(1, 2))).frame.pixels[0], 0);
        assert_eq!(frame_of(router.handle(&video(1, 3))).frame.pixels[0], 1);
        // Stream two's decoder has seen nothing yet, so it is still at zero.
        let second = frame_of(router.handle(&video(2, 2)));
        assert_eq!(second.frame.pixels[0], 0);
        assert_eq!(second.client_id, 21);
        assert_eq!(second.surface_id, 22);
        assert_eq!(frame_of(router.handle(&video(1, 4))).frame.pixels[0], 2);
    }

    #[test]
    fn stream_end_tears_down_only_its_own_stream() {
        let mut router = fake_router();
        router.handle(&stream_config(1, 1, 11, 12, &CODEC_CONFIG));
        router.handle(&stream_config(2, 1, 21, 22, &CODEC_CONFIG));
        router.handle(&video(1, 2));

        assert_eq!(
            router.handle(&packet(MediaKind::StreamEnd, 1, 3, Vec::new())),
            vec![StreamEvent::Ended { stream_id: 1 }]
        );
        assert_eq!(router.live_streams(), 1);
        assert!(router.handle(&video(1, 4)).is_empty());
        assert_eq!(frame_of(router.handle(&video(2, 2))).frame.pixels[0], 0);
        assert!(
            router
                .handle(&packet(MediaKind::StreamEnd, 1, 5, Vec::new()))
                .is_empty()
        );
    }

    #[test]
    fn stream_end_flushes_frames_the_pipeline_had_not_yet_delivered() {
        let mut router = StreamRouter::new(Box::new(|config: &DecoderConfig| {
            Ok(Box::new(BufferingDecoder::new(config.clone())?) as Box<dyn Decoder>)
        }));
        router.handle(&stream_config(1, 1, 11, 12, &CODEC_CONFIG));
        // Priming: the pipeline buffers this access unit's frame internally
        // and reports nothing yet.
        assert!(router.handle(&video(1, 2)).is_empty());

        // The window goes idle and its toplevel closes with a picture still
        // sitting undelivered inside the decoder. `end` must surface it
        // rather than discard it, and the `Ended` event must come after it.
        assert_eq!(
            router.handle(&packet(MediaKind::StreamEnd, 1, 3, Vec::new())),
            vec![
                StreamEvent::Frame(StreamFrame {
                    stream_id: 1,
                    client_id: 11,
                    surface_id: 12,
                    timestamp_us: 2000,
                    frame: DecodedFrame {
                        width: 4,
                        height: 2,
                        pixels: vec![0; 4 * 2 * 4],
                        decode_time: Duration::ZERO,
                    },
                }),
                StreamEvent::Ended { stream_id: 1 },
            ]
        );
        assert_eq!(router.live_streams(), 0);
    }

    #[test]
    fn a_new_codec_configuration_resets_that_stream_decoder() {
        let mut router = fake_router();
        router.handle(&stream_config(1, 1, 11, 12, &CODEC_CONFIG));
        router.handle(&video(1, 2));
        assert_eq!(frame_of(router.handle(&video(1, 3))).frame.pixels[0], 1);

        // A replay of the identical configuration must not disturb the decoder.
        router.handle(&stream_config(1, 4, 11, 12, &CODEC_CONFIG));
        assert_eq!(frame_of(router.handle(&video(1, 5))).frame.pixels[0], 2);

        router.handle(&stream_config(1, 6, 11, 12, &OTHER_CODEC_CONFIG));
        assert_eq!(router.live_streams(), 1);
        assert_eq!(frame_of(router.handle(&video(1, 7))).frame.pixels[0], 0);
    }

    #[test]
    fn a_resized_stream_configuration_resets_that_stream_decoder() {
        // The identical codec_config is reused deliberately: only the coded
        // dimensions differ, so this test fails unless the router's reset
        // interlock also compares width/height, not just codec_config.
        let mut router = fake_router();
        router.handle(&stream_config(1, 1, 11, 12, &CODEC_CONFIG));
        assert_eq!(frame_of(router.handle(&video(1, 2))).frame.pixels[0], 0);
        assert_eq!(frame_of(router.handle(&video(1, 3))).frame.pixels[0], 1);

        router.handle(&stream_config_sized(1, 4, 11, 12, &CODEC_CONFIG, 4, 6));
        assert_eq!(router.live_streams(), 1);
        let resized = frame_of(router.handle(&video(1, 5)));
        assert_eq!(resized.frame.pixels[0], 0);
        assert_eq!((resized.frame.width, resized.frame.height), (4, 6));
    }

    #[test]
    fn an_empty_video_payload_is_dropped_without_disturbing_the_decoder() {
        let mut router = fake_router();
        router.handle(&stream_config(1, 1, 11, 12, &CODEC_CONFIG));
        assert!(
            router
                .handle(&packet(MediaKind::Video, 1, 2, Vec::new()))
                .is_empty()
        );
        assert_eq!(router.live_streams(), 1);
        // The decoder never saw the empty packet, so it is still at zero.
        assert_eq!(frame_of(router.handle(&video(1, 3))).frame.pixels[0], 0);
    }

    #[test]
    fn malformed_stream_config_leaves_the_stream_unconfigured() {
        let mut router = fake_router();
        router.handle(&packet(MediaKind::StreamConfig, 1, 1, vec![9, 9, 9]));
        assert_eq!(router.live_streams(), 0);
        assert!(router.handle(&video(1, 2)).is_empty());
    }

    /// Reports a priming gap and then a hard failure, so the router's two
    /// non-frame decode outcomes can be told apart.
    struct StubbornDecoder {
        config: DecoderConfig,
        calls: u64,
    }

    impl Decoder for StubbornDecoder {
        fn config(&self) -> &DecoderConfig {
            &self.config
        }

        fn decode(&mut self, _access_unit: &[u8]) -> Result<Vec<DecodedFrame>, DecoderError> {
            self.calls += 1;
            match self.calls {
                1 => Ok(Vec::new()),
                _ => Err(DecoderError::ProcessExited),
            }
        }

        fn drain(&mut self) -> Vec<DecodedFrame> {
            Vec::new()
        }

        fn reconfigure(&mut self, config: DecoderConfig) -> Result<(), DecoderError> {
            self.config = config.validate()?;
            Ok(())
        }

        fn metrics(&self) -> DecoderMetrics {
            DecoderMetrics::default()
        }
    }

    #[test]
    fn priming_yields_nothing_and_a_decode_failure_asks_for_a_keyframe() {
        let mut router = StreamRouter::new(Box::new(|config: &DecoderConfig| {
            Ok(Box::new(StubbornDecoder {
                config: config.clone().validate()?,
                calls: 0,
            }) as Box<dyn Decoder>)
        }));
        router.handle(&stream_config(2, 1, 21, 22, &CODEC_CONFIG));
        router.handle(&stream_config(1, 1, 11, 12, &CODEC_CONFIG));
        // Priming is silent, and it is not a failure.
        assert!(router.handle(&video(1, 2)).is_empty());
        assert_eq!(router.live_streams(), 2);

        assert_eq!(
            router.handle(&video(1, 3)),
            vec![StreamEvent::DecodeFailed { stream_id: 1 }]
        );
        // Only the broken stream's decoder is discarded; the keyframe request
        // the client sends next brings a fresh `stream_config` that rebuilds
        // it, and the other stream is untouched throughout.
        assert_eq!(router.live_streams(), 1);
        assert!(router.handle(&video(1, 4)).is_empty());
        router.handle(&stream_config(1, 5, 11, 12, &CODEC_CONFIG));
        assert_eq!(router.live_streams(), 2);
        assert!(router.handle(&video(1, 6)).is_empty());
    }

    #[test]
    fn a_failing_decoder_factory_leaves_the_stream_unconfigured() {
        let mut router = StreamRouter::new(Box::new(|_: &DecoderConfig| {
            Err(DecoderError::InvalidConfig)
        }));
        router.handle(&stream_config(1, 1, 11, 12, &CODEC_CONFIG));
        assert_eq!(router.live_streams(), 0);
        assert!(router.handle(&video(1, 2)).is_empty());
    }

    /// Buffers exactly one frame internally instead of returning it
    /// immediately, so `end`'s flush-before-`Ended` behaviour can be
    /// exercised: `decode` reports priming (nothing ready yet) and `drain`
    /// later hands back what was buffered.
    struct BufferingDecoder {
        config: DecoderConfig,
        buffered: Option<DecodedFrame>,
    }

    impl BufferingDecoder {
        fn new(config: DecoderConfig) -> Result<Self, DecoderError> {
            Ok(Self {
                config: config.validate()?,
                buffered: None,
            })
        }
    }

    impl Decoder for BufferingDecoder {
        fn config(&self) -> &DecoderConfig {
            &self.config
        }

        fn decode(&mut self, access_unit: &[u8]) -> Result<Vec<DecodedFrame>, DecoderError> {
            if access_unit.is_empty() {
                return Err(DecoderError::EmptyAccessUnit);
            }
            self.buffered = Some(DecodedFrame {
                width: self.config.width,
                height: self.config.height,
                pixels: vec![0; self.config.frame_len()],
                decode_time: Duration::ZERO,
            });
            Ok(Vec::new())
        }

        fn drain(&mut self) -> Vec<DecodedFrame> {
            self.buffered.take().into_iter().collect()
        }

        fn reconfigure(&mut self, config: DecoderConfig) -> Result<(), DecoderError> {
            self.config = config.validate()?;
            self.buffered = None;
            Ok(())
        }

        fn metrics(&self) -> DecoderMetrics {
            DecoderMetrics::default()
        }
    }
}
