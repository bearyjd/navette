use std::collections::HashMap;

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

    /// Feeds one packet through the router. At most one event comes back: a
    /// packet can bootstrap a stream, produce a frame, end a stream or, when
    /// the decode pipeline is still priming, produce nothing at all.
    pub fn handle(&mut self, packet: &MediaPacket) -> Option<StreamEvent> {
        match packet.header.kind {
            MediaKind::StreamConfig => {
                self.configure(packet);
                None
            }
            MediaKind::Video => self.decode(packet),
            MediaKind::StreamEnd => self.end(packet.header.stream_id),
            MediaKind::Metrics => None,
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
                    },
                );
            }
            Err(error) => tracing::error!(stream_id, %error, "failed to create decoder"),
        }
    }

    fn decode(&mut self, packet: &MediaPacket) -> Option<StreamEvent> {
        let stream_id = packet.header.stream_id;
        let Some(stream) = self.streams.get_mut(&stream_id) else {
            tracing::warn!(stream_id, "dropping video for an unconfigured stream");
            return None;
        };
        match stream.decoder.decode(&packet.payload) {
            Ok(Some(frame)) => Some(StreamEvent::Frame(StreamFrame {
                stream_id,
                client_id: stream.client_id,
                surface_id: stream.surface_id,
                timestamp_us: packet.header.timestamp_us,
                frame,
            })),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(stream_id, %error, "decode failed; requesting a keyframe");
                // Every decode error means this stream's FFmpeg process is
                // unusable, so the decoder goes with it. The keyframe request
                // the client sends next makes the bridge re-emit SPS/PPS as a
                // fresh `stream_config`, which rebuilds the decoder — the rest
                // of the session keeps running throughout.
                self.streams.remove(&stream_id);
                Some(StreamEvent::DecodeFailed { stream_id })
            }
        }
    }

    fn end(&mut self, stream_id: u64) -> Option<StreamEvent> {
        if self.streams.remove(&stream_id).is_none() {
            tracing::debug!(stream_id, "ignoring end of an unconfigured stream");
            return None;
        }
        tracing::debug!(stream_id, "stream ended");
        Some(StreamEvent::Ended { stream_id })
    }
}

#[cfg(test)]
mod tests {
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
        let payload = StreamConfig {
            client_id,
            surface_id,
            codec_config: codec_config.to_vec(),
        }
        .encode()
        .unwrap();
        packet(MediaKind::StreamConfig, stream_id, sequence, payload)
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

    fn frame_of(event: Option<StreamEvent>) -> StreamFrame {
        match event {
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
                .is_none()
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
        assert!(router.handle(&video(7, 1)).is_none());
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
            Some(StreamEvent::Ended { stream_id: 1 })
        );
        assert_eq!(router.live_streams(), 1);
        assert!(router.handle(&video(1, 4)).is_none());
        assert_eq!(frame_of(router.handle(&video(2, 2))).frame.pixels[0], 0);
        assert!(
            router
                .handle(&packet(MediaKind::StreamEnd, 1, 5, Vec::new()))
                .is_none()
        );
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
    fn malformed_stream_config_leaves_the_stream_unconfigured() {
        let mut router = fake_router();
        router.handle(&packet(MediaKind::StreamConfig, 1, 1, vec![9, 9, 9]));
        assert_eq!(router.live_streams(), 0);
        assert!(router.handle(&video(1, 2)).is_none());
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

        fn decode(&mut self, _access_unit: &[u8]) -> Result<Option<DecodedFrame>, DecoderError> {
            self.calls += 1;
            match self.calls {
                1 => Ok(None),
                _ => Err(DecoderError::ProcessExited),
            }
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
        assert!(router.handle(&video(1, 2)).is_none());
        assert_eq!(router.live_streams(), 2);

        assert_eq!(
            router.handle(&video(1, 3)),
            Some(StreamEvent::DecodeFailed { stream_id: 1 })
        );
        // Only the broken stream's decoder is discarded; the keyframe request
        // the client sends next brings a fresh `stream_config` that rebuilds
        // it, and the other stream is untouched throughout.
        assert_eq!(router.live_streams(), 1);
        assert!(router.handle(&video(1, 4)).is_none());
        router.handle(&stream_config(1, 5, 11, 12, &CODEC_CONFIG));
        assert_eq!(router.live_streams(), 2);
        assert!(router.handle(&video(1, 6)).is_none());
    }

    #[test]
    fn a_failing_decoder_factory_leaves_the_stream_unconfigured() {
        let mut router = StreamRouter::new(Box::new(|_: &DecoderConfig| {
            Err(DecoderError::InvalidConfig)
        }));
        router.handle(&stream_config(1, 1, 11, 12, &CODEC_CONFIG));
        assert_eq!(router.live_streams(), 0);
        assert!(router.handle(&video(1, 2)).is_none());
    }
}
