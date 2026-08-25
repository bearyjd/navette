use std::fmt;

use serde::{Deserialize, Serialize};

pub const MEDIA_WEBSOCKET_SUBPROTOCOL: &str = "navette.media.v1";
pub const MEDIA_MAGIC: [u8; 4] = *b"NVTM";
pub const MEDIA_VERSION: u16 = 1;
pub const MEDIA_HEADER_LEN: usize = 44;
pub const MAX_MEDIA_PAYLOAD: usize = 16 * 1024 * 1024;
pub const MAX_INPUT_MESSAGE: usize = 16 * 1024;
pub const STREAM_CONFIG_VERSION: u8 = 1;
pub const STREAM_CONFIG_PREFIX_LEN: usize = 21;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MediaKind {
    StreamConfig = 1,
    Video = 2,
    StreamEnd = 3,
    Metrics = 4,
}

impl TryFrom<u8> for MediaKind {
    type Error = MediaDecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::StreamConfig),
            2 => Ok(Self::Video),
            3 => Ok(Self::StreamEnd),
            4 => Ok(Self::Metrics),
            other => Err(MediaDecodeError::UnknownKind(other)),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MediaFlags(u8);

impl MediaFlags {
    const KEYFRAME: u8 = 1;
    const DISCONTINUITY: u8 = 2;
    const KNOWN: u8 = Self::KEYFRAME | Self::DISCONTINUITY;

    pub const fn new(keyframe: bool, discontinuity: bool) -> Self {
        Self((keyframe as u8) | ((discontinuity as u8) << 1))
    }

    pub const fn keyframe(self) -> bool {
        self.0 & Self::KEYFRAME != 0
    }

    pub const fn discontinuity(self) -> bool {
        self.0 & Self::DISCONTINUITY != 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaHeader {
    pub kind: MediaKind,
    pub flags: MediaFlags,
    pub stream_id: u64,
    pub sequence: u64,
    pub timestamp_us: u64,
    pub payload_len: u32,
    pub width: u32,
    pub height: u32,
}

impl MediaHeader {
    pub fn encode(&self) -> [u8; MEDIA_HEADER_LEN] {
        let mut output = [0; MEDIA_HEADER_LEN];
        output[..4].copy_from_slice(&MEDIA_MAGIC);
        output[4..6].copy_from_slice(&MEDIA_VERSION.to_be_bytes());
        output[6] = self.kind as u8;
        output[7] = self.flags.0;
        output[8..16].copy_from_slice(&self.stream_id.to_be_bytes());
        output[16..24].copy_from_slice(&self.sequence.to_be_bytes());
        output[24..32].copy_from_slice(&self.timestamp_us.to_be_bytes());
        output[32..36].copy_from_slice(&self.payload_len.to_be_bytes());
        output[36..40].copy_from_slice(&self.width.to_be_bytes());
        output[40..44].copy_from_slice(&self.height.to_be_bytes());
        output
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MediaDecodeError> {
        if bytes.len() < MEDIA_HEADER_LEN {
            return Err(MediaDecodeError::TruncatedHeader);
        }
        if bytes[..4] != MEDIA_MAGIC {
            return Err(MediaDecodeError::InvalidMagic);
        }
        let version = u16::from_be_bytes([bytes[4], bytes[5]]);
        if version != MEDIA_VERSION {
            return Err(MediaDecodeError::UnsupportedVersion(version));
        }
        if bytes[7] & !MediaFlags::KNOWN != 0 {
            return Err(MediaDecodeError::UnknownFlags(bytes[7]));
        }
        let payload_len = u32::from_be_bytes(bytes[32..36].try_into().expect("fixed slice"));
        if payload_len as usize > MAX_MEDIA_PAYLOAD {
            return Err(MediaDecodeError::PayloadTooLarge(payload_len));
        }
        Ok(Self {
            kind: MediaKind::try_from(bytes[6])?,
            flags: MediaFlags(bytes[7]),
            stream_id: u64::from_be_bytes(bytes[8..16].try_into().expect("fixed slice")),
            sequence: u64::from_be_bytes(bytes[16..24].try_into().expect("fixed slice")),
            timestamp_us: u64::from_be_bytes(bytes[24..32].try_into().expect("fixed slice")),
            payload_len,
            width: u32::from_be_bytes(bytes[36..40].try_into().expect("fixed slice")),
            height: u32::from_be_bytes(bytes[40..44].try_into().expect("fixed slice")),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaPacket {
    pub header: MediaHeader,
    pub payload: Vec<u8>,
}

/// Bounded codec bootstrap and scene identity carried by `stream_config`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamConfig {
    pub client_id: u64,
    pub surface_id: u64,
    /// Annex-B SPS/PPS bytes for the H.264 stream.
    pub codec_config: Vec<u8>,
}

impl StreamConfig {
    pub fn encode(&self) -> Result<Vec<u8>, MediaDecodeError> {
        if self.codec_config.len() > MAX_MEDIA_PAYLOAD - STREAM_CONFIG_PREFIX_LEN
            || self.codec_config.len() > u32::MAX as usize
        {
            return Err(MediaDecodeError::PayloadTooLarge(u32::MAX));
        }
        let mut output = Vec::with_capacity(STREAM_CONFIG_PREFIX_LEN + self.codec_config.len());
        output.push(STREAM_CONFIG_VERSION);
        output.extend_from_slice(&self.client_id.to_be_bytes());
        output.extend_from_slice(&self.surface_id.to_be_bytes());
        output.extend_from_slice(&(self.codec_config.len() as u32).to_be_bytes());
        output.extend_from_slice(&self.codec_config);
        Ok(output)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MediaDecodeError> {
        if bytes.len() < STREAM_CONFIG_PREFIX_LEN {
            return Err(MediaDecodeError::TruncatedStreamConfig);
        }
        if bytes[0] != STREAM_CONFIG_VERSION {
            return Err(MediaDecodeError::UnsupportedStreamConfigVersion(bytes[0]));
        }
        let codec_len = u32::from_be_bytes(bytes[17..21].try_into().expect("fixed slice"));
        let expected = STREAM_CONFIG_PREFIX_LEN
            .checked_add(codec_len as usize)
            .ok_or(MediaDecodeError::LengthMismatch)?;
        if bytes.len() != expected || expected > MAX_MEDIA_PAYLOAD {
            return Err(MediaDecodeError::LengthMismatch);
        }
        Ok(Self {
            client_id: u64::from_be_bytes(bytes[1..9].try_into().expect("fixed slice")),
            surface_id: u64::from_be_bytes(bytes[9..17].try_into().expect("fixed slice")),
            codec_config: bytes[STREAM_CONFIG_PREFIX_LEN..].to_vec(),
        })
    }
}

impl MediaPacket {
    pub fn new(mut header: MediaHeader, payload: Vec<u8>) -> Result<Self, MediaDecodeError> {
        if payload.len() > MAX_MEDIA_PAYLOAD || payload.len() > u32::MAX as usize {
            return Err(MediaDecodeError::PayloadTooLarge(
                u32::try_from(payload.len()).unwrap_or(u32::MAX),
            ));
        }
        header.payload_len = payload.len() as u32;
        Ok(Self { header, payload })
    }

    pub fn encode(&self) -> Result<Vec<u8>, MediaDecodeError> {
        self.validate()?;
        let mut output = Vec::with_capacity(MEDIA_HEADER_LEN + self.payload.len());
        output.extend_from_slice(&self.header.encode());
        output.extend_from_slice(&self.payload);
        Ok(output)
    }

    pub fn validate(&self) -> Result<(), MediaDecodeError> {
        if self.payload.len() > MAX_MEDIA_PAYLOAD {
            return Err(MediaDecodeError::PayloadTooLarge(
                u32::try_from(self.payload.len()).unwrap_or(u32::MAX),
            ));
        }
        if self.payload.len() != self.header.payload_len as usize {
            return Err(MediaDecodeError::LengthMismatch);
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MediaDecodeError> {
        let header = MediaHeader::decode(bytes)?;
        let expected = MEDIA_HEADER_LEN
            .checked_add(header.payload_len as usize)
            .ok_or(MediaDecodeError::LengthMismatch)?;
        if bytes.len() != expected {
            return Err(MediaDecodeError::LengthMismatch);
        }
        Ok(Self {
            header,
            payload: bytes[MEDIA_HEADER_LEN..].to_vec(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MediaDecodeError {
    TruncatedHeader,
    InvalidMagic,
    UnsupportedVersion(u16),
    UnknownKind(u8),
    UnknownFlags(u8),
    PayloadTooLarge(u32),
    LengthMismatch,
    TruncatedStreamConfig,
    UnsupportedStreamConfigVersion(u8),
}

impl fmt::Display for MediaDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for MediaDecodeError {}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaInput {
    PointerMotion {
        client_id: u64,
        surface_id: u64,
        x: f64,
        y: f64,
    },
    PointerButton {
        client_id: u64,
        surface_id: u64,
        button: u32,
        pressed: bool,
    },
    PointerAxis {
        client_id: u64,
        surface_id: u64,
        horizontal: f64,
        vertical: f64,
    },
    KeyboardKey {
        client_id: u64,
        surface_id: u64,
        keycode: u32,
        pressed: bool,
    },
    KeyboardModifiers {
        client_id: u64,
        surface_id: u64,
        ctrl: bool,
        alt: bool,
        shift: bool,
        caps_lock: bool,
        logo: bool,
        num_lock: bool,
        layout_index: u32,
    },
    ViewportResize {
        width: u32,
        height: u32,
    },
    RequestKeyframe,
}

impl MediaInput {
    pub fn validate(&self) -> Result<(), InputValidationError> {
        match self {
            Self::PointerMotion { x, y, .. } if !x.is_finite() || !y.is_finite() => {
                Err(InputValidationError::NonFiniteCoordinate)
            }
            Self::PointerAxis {
                horizontal,
                vertical,
                ..
            } if !horizontal.is_finite() || !vertical.is_finite() => {
                Err(InputValidationError::NonFiniteCoordinate)
            }
            Self::PointerButton { button, .. } if !(0x110..=0x11f).contains(button) => {
                Err(InputValidationError::ButtonOutOfRange(*button))
            }
            Self::KeyboardKey { keycode, .. } if *keycode > 767 => {
                Err(InputValidationError::KeyOutOfRange(*keycode))
            }
            Self::ViewportResize { width, height }
                if !(320..=3840).contains(width) || !(240..=2160).contains(height) =>
            {
                Err(InputValidationError::ViewportOutOfRange)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputValidationError {
    NonFiniteCoordinate,
    ButtonOutOfRange(u32),
    KeyOutOfRange(u32),
    ViewportOutOfRange,
}

impl fmt::Display for InputValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for InputValidationError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaServerMessage {
    Error { code: String, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> MediaHeader {
        MediaHeader {
            kind: MediaKind::Video,
            flags: MediaFlags::new(true, true),
            stream_id: 7,
            sequence: 9,
            timestamp_us: 11,
            payload_len: 0,
            width: 1920,
            height: 1080,
        }
    }

    #[test]
    fn media_packet_round_trips_in_network_byte_order() {
        let packet = MediaPacket::new(header(), vec![0, 0, 0, 1, 0x65]).unwrap();
        let encoded = packet.encode().unwrap();
        assert_eq!(&encoded[..4], b"NVTM");
        assert_eq!(&encoded[4..6], &[0, 1]);
        assert_eq!(encoded[6], MediaKind::Video as u8);
        assert_eq!(encoded[7], 3);
        assert_eq!(MediaPacket::decode(&encoded).unwrap(), packet);
    }

    #[test]
    fn stream_config_round_trips_with_surface_identity() {
        let config = StreamConfig {
            client_id: 11,
            surface_id: 12,
            codec_config: vec![0, 0, 0, 1, 0x67],
        };
        assert_eq!(
            StreamConfig::decode(&config.encode().unwrap()).unwrap(),
            config
        );
        assert_eq!(
            StreamConfig::decode(&[]),
            Err(MediaDecodeError::TruncatedStreamConfig)
        );
    }

    #[test]
    fn malformed_or_oversized_packets_are_rejected_before_payload_copy() {
        assert_eq!(
            MediaPacket::decode(b"short").unwrap_err(),
            MediaDecodeError::TruncatedHeader
        );
        let mut encoded = MediaPacket::new(header(), vec![1])
            .unwrap()
            .encode()
            .unwrap();
        encoded[0] = b'X';
        assert_eq!(
            MediaPacket::decode(&encoded).unwrap_err(),
            MediaDecodeError::InvalidMagic
        );
        let mut encoded = MediaPacket::new(header(), vec![1])
            .unwrap()
            .encode()
            .unwrap();
        encoded[32..36].copy_from_slice(&((MAX_MEDIA_PAYLOAD as u32) + 1).to_be_bytes());
        assert!(matches!(
            MediaPacket::decode(&encoded),
            Err(MediaDecodeError::PayloadTooLarge(_))
        ));
    }

    #[test]
    fn input_validation_rejects_adversarial_values() {
        assert_eq!(
            MediaInput::PointerMotion {
                client_id: 1,
                surface_id: 2,
                x: f64::NAN,
                y: 0.0,
            }
            .validate(),
            Err(InputValidationError::NonFiniteCoordinate)
        );
        assert_eq!(
            MediaInput::ViewportResize {
                width: 10,
                height: 10,
            }
            .validate(),
            Err(InputValidationError::ViewportOutOfRange)
        );
    }
}
