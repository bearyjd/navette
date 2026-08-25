use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use thiserror::Error;

const OUTPUT_QUEUE_CAPACITY: usize = 4;

/// FFmpeg's H.264 parser and decoder hold a small, constant number of access
/// units before the first frame appears on stdout (measured at three for the
/// bridge's `-bf 0` streams). `decode` waits this long for a frame before
/// reporting that the pipeline is still priming, so the cost is paid a handful
/// of times per stream and never in steady state.
const FRAME_TIMEOUT: Duration = Duration::from_millis(100);

/// Coded dimensions and codec bootstrap for one H.264 stream.
///
/// The dimensions come from `MediaHeader::width`/`height`, which the bridge
/// fills in from its encoder configuration for every packet it publishes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecoderConfig {
    pub width: u32,
    pub height: u32,
    /// Annex-B SPS/PPS that bootstraps the stream.
    pub codec_config: Vec<u8>,
}

impl DecoderConfig {
    pub fn validate(self) -> Result<Self, DecoderError> {
        if self.width < 2
            || self.height < 2
            || self.width > 8192
            || self.height > 8192
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
            || self.codec_config.is_empty()
        {
            return Err(DecoderError::InvalidConfig);
        }
        Ok(self)
    }

    /// Byte length of one tightly packed BGRA frame at these dimensions.
    pub fn frame_len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}

/// One decoded picture in the same tightly packed BGRA layout that
/// `navette_bridge::Frame` uses on the encode side.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub decode_time: Duration,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DecoderMetrics {
    pub access_units_submitted: u64,
    pub frames_decoded: u64,
    pub frames_dropped: u64,
    pub bytes_submitted: u64,
    pub last_decode_time: Duration,
}

pub trait Decoder: Send {
    fn config(&self) -> &DecoderConfig;

    /// Submits one Annex-B access unit and returns the next decoded frame.
    ///
    /// Returns `Ok(None)` while FFmpeg's pipeline is still priming: that is a
    /// normal part of stream startup, not a decode failure, and it must not be
    /// answered with a keyframe request. Once primed the decoder produces one
    /// frame per access unit.
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<DecodedFrame>, DecoderError>;

    fn reconfigure(&mut self, config: DecoderConfig) -> Result<(), DecoderError>;

    fn metrics(&self) -> DecoderMetrics;
}

/// Decodes H.264 by piping Annex-B access units through an `ffmpeg`
/// subprocess and reading raw BGRA frames back.
pub struct FfmpegDecoder {
    executable: PathBuf,
    config: DecoderConfig,
    process: Option<DecoderProcess>,
    metrics: DecoderMetrics,
}

impl FfmpegDecoder {
    pub fn new(
        executable: impl Into<PathBuf>,
        config: DecoderConfig,
    ) -> Result<Self, DecoderError> {
        let executable = executable.into();
        let config = config.validate()?;
        let process = DecoderProcess::spawn(&executable, &config)?;
        Ok(Self {
            executable,
            config,
            process: Some(process),
            metrics: DecoderMetrics::default(),
        })
    }

    fn restart(&mut self) -> Result<(), DecoderError> {
        self.process.take();
        self.process = Some(DecoderProcess::spawn(&self.executable, &self.config)?);
        Ok(())
    }
}

impl Decoder for FfmpegDecoder {
    fn config(&self) -> &DecoderConfig {
        &self.config
    }

    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<DecodedFrame>, DecoderError> {
        if access_unit.is_empty() {
            return Err(DecoderError::EmptyAccessUnit);
        }
        self.metrics.access_units_submitted = self.metrics.access_units_submitted.saturating_add(1);
        self.metrics.bytes_submitted = self
            .metrics
            .bytes_submitted
            .saturating_add(access_unit.len() as u64);
        let started = Instant::now();
        let process = self.process.as_mut().ok_or(DecoderError::ProcessExited)?;
        process
            .stdin
            .write_all(access_unit)
            .map_err(DecoderError::Write)?;
        process.stdin.flush().map_err(DecoderError::Write)?;
        let pixels = match process.output.recv_timeout(FRAME_TIMEOUT) {
            Ok(pixels) => pixels,
            Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(DecoderError::ProcessExited),
        };
        let decode_time = started.elapsed();
        self.metrics.frames_decoded = self.metrics.frames_decoded.saturating_add(1);
        self.metrics.frames_dropped = process.dropped.load(Ordering::Relaxed);
        self.metrics.last_decode_time = decode_time;
        Ok(Some(DecodedFrame {
            width: self.config.width,
            height: self.config.height,
            pixels,
            decode_time,
        }))
    }

    fn reconfigure(&mut self, config: DecoderConfig) -> Result<(), DecoderError> {
        self.config = config.validate()?;
        self.restart()
    }

    fn metrics(&self) -> DecoderMetrics {
        self.metrics.clone()
    }
}

/// Deterministic decoder that never spawns a subprocess, for headless tests.
///
/// Every access unit yields a solid-colour frame whose channel value follows
/// the decoder's own submission counter, so a test can tell one stream's
/// decoder state from another's.
pub struct FakeDecoder {
    config: DecoderConfig,
    sequence: u64,
    metrics: DecoderMetrics,
}

impl FakeDecoder {
    pub fn new(config: DecoderConfig) -> Result<Self, DecoderError> {
        Ok(Self {
            config: config.validate()?,
            sequence: 0,
            metrics: DecoderMetrics::default(),
        })
    }
}

impl Decoder for FakeDecoder {
    fn config(&self) -> &DecoderConfig {
        &self.config
    }

    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<DecodedFrame>, DecoderError> {
        if access_unit.is_empty() {
            return Err(DecoderError::EmptyAccessUnit);
        }
        self.metrics.access_units_submitted = self.metrics.access_units_submitted.saturating_add(1);
        self.metrics.bytes_submitted = self
            .metrics
            .bytes_submitted
            .saturating_add(access_unit.len() as u64);
        let shade = self.sequence as u8;
        self.sequence = self.sequence.saturating_add(1);
        self.metrics.frames_decoded = self.metrics.frames_decoded.saturating_add(1);
        Ok(Some(DecodedFrame {
            width: self.config.width,
            height: self.config.height,
            pixels: vec![shade; self.config.frame_len()],
            decode_time: Duration::ZERO,
        }))
    }

    fn reconfigure(&mut self, config: DecoderConfig) -> Result<(), DecoderError> {
        self.config = config.validate()?;
        self.sequence = 0;
        Ok(())
    }

    fn metrics(&self) -> DecoderMetrics {
        self.metrics.clone()
    }
}

struct DecoderProcess {
    child: Child,
    stdin: ChildStdin,
    output: Receiver<Vec<u8>>,
    dropped: Arc<AtomicU64>,
    reader: Option<JoinHandle<()>>,
}

impl DecoderProcess {
    fn spawn(executable: &Path, config: &DecoderConfig) -> Result<Self, DecoderError> {
        let mut child = ffmpeg_command(executable, config)
            .spawn()
            .map_err(DecoderError::Spawn)?;
        let Some((stdin, stdout)) = child.stdin.take().zip(child.stdout.take()) else {
            drop(child.kill());
            drop(child.wait());
            return Err(DecoderError::MissingPipe);
        };
        let (sender, output) = mpsc::sync_channel(OUTPUT_QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let reader_dropped = Arc::clone(&dropped);
        let frame_len = config.frame_len();
        let reader = thread::spawn(move || read_frames(stdout, frame_len, sender, reader_dropped));
        // Assembled before the first write so that a failure here tears the
        // subprocess down through `Drop` instead of orphaning it.
        let mut process = Self {
            child,
            stdin,
            output,
            dropped,
            reader: Some(reader),
        };
        // Bootstrapping with SPS/PPS up front means the first access unit does
        // not have to carry them, whatever the bridge replayed on attach.
        process
            .stdin
            .write_all(&config.codec_config)
            .map_err(DecoderError::Write)?;
        process.stdin.flush().map_err(DecoderError::Write)?;
        Ok(process)
    }
}

impl Drop for DecoderProcess {
    fn drop(&mut self) {
        drop(self.stdin.flush());
        let pid = Pid::from_raw(i32::try_from(self.child.id()).unwrap_or(i32::MAX));
        let _ = killpg(pid, Signal::SIGTERM);
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = killpg(pid, Signal::SIGKILL);
            drop(self.child.wait());
        }
        if let Some(reader) = self.reader.take() {
            drop(reader.join());
        }
    }
}

/// Builds the decode pipeline.
///
/// `-s` pins the output to the coded dimensions the bridge advertised, so
/// every frame on stdout is exactly `width * height * 4` bytes and frame
/// boundaries never have to be guessed. `-probesize`/`-analyzeduration` stop
/// `avformat` from swallowing the head of the stream while it estimates a
/// frame rate, and `-flags low_delay` keeps the reorder buffer at the minimum
/// the bridge's B-frame-free streams allow.
fn ffmpeg_command(executable: &Path, config: &DecoderConfig) -> Command {
    let mut command = Command::new(executable);
    command
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-probesize",
            "32",
            "-analyzeduration",
            "0",
            "-flags",
            "low_delay",
            "-f",
            "h264",
            "-i",
            "pipe:0",
            "-an",
            "-s",
            &format!("{}x{}", config.width, config.height),
            "-pix_fmt",
            "bgra",
            "-flush_packets",
            "1",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    command
}

fn read_frames(
    mut stdout: ChildStdout,
    frame_len: usize,
    sender: SyncSender<Vec<u8>>,
    dropped: Arc<AtomicU64>,
) {
    loop {
        let mut frame = vec![0_u8; frame_len];
        let mut filled = 0;
        while filled < frame_len {
            match stdout.read(&mut frame[filled..]) {
                // A short read is normal on a pipe; end of file means FFmpeg
                // exited, and a partial frame at that point is discarded.
                Ok(0) | Err(_) => return,
                Ok(length) => filled += length,
            }
        }
        if sender.try_send(frame).is_err() {
            dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Debug, Error)]
pub enum DecoderError {
    #[error("invalid decoder configuration")]
    InvalidConfig,
    #[error("access unit is empty")]
    EmptyAccessUnit,
    #[error("failed to spawn FFmpeg: {0}")]
    Spawn(std::io::Error),
    #[error("FFmpeg pipe is unavailable")]
    MissingPipe,
    #[error("failed to write FFmpeg input: {0}")]
    Write(std::io::Error),
    #[error("FFmpeg exited")]
    ProcessExited,
}

#[cfg(test)]
mod tests {
    use navette_bridge::{Encoder, EncoderBackend, EncoderConfig, FfmpegEncoder, Frame};

    use super::*;

    fn config(width: u32, height: u32) -> DecoderConfig {
        DecoderConfig {
            width,
            height,
            codec_config: vec![0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 1],
        }
    }

    fn encoder_config(width: u32, height: u32) -> EncoderConfig {
        EncoderConfig {
            width,
            height,
            frame_rate: 30,
            bitrate: 500_000,
            keyframe_interval: 30,
        }
    }

    fn frame(width: u32, height: u32) -> Frame {
        Frame {
            width,
            height,
            pixels: vec![0; width as usize * height as usize * 4],
        }
    }

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    #[test]
    fn configuration_rejects_unusable_dimensions_and_missing_bootstrap() {
        assert!(matches!(
            config(65, 64).validate(),
            Err(DecoderError::InvalidConfig)
        ));
        assert!(matches!(
            config(0, 64).validate(),
            Err(DecoderError::InvalidConfig)
        ));
        assert!(matches!(
            config(9000, 64).validate(),
            Err(DecoderError::InvalidConfig)
        ));
        assert!(matches!(
            DecoderConfig {
                codec_config: Vec::new(),
                ..config(64, 64)
            }
            .validate(),
            Err(DecoderError::InvalidConfig)
        ));
        assert_eq!(config(64, 32).validate().unwrap().frame_len(), 64 * 32 * 4);
    }

    #[test]
    fn fake_decoder_is_deterministic_and_resets_on_reconfigure() {
        let mut first = FakeDecoder::new(config(4, 2)).unwrap();
        let mut second = FakeDecoder::new(config(4, 2)).unwrap();
        assert_eq!(
            first.decode(&[0, 0, 0, 1, 0x65]).unwrap(),
            second.decode(&[0, 0, 0, 1, 0x41]).unwrap()
        );
        let next = first.decode(&[0, 0, 0, 1, 0x41]).unwrap().unwrap();
        assert_eq!(next.pixels, vec![1; 4 * 2 * 4]);
        assert_eq!(next.width, 4);
        assert_eq!(next.height, 2);

        first.reconfigure(config(2, 2)).unwrap();
        let reset = first.decode(&[0, 0, 0, 1, 0x65]).unwrap().unwrap();
        assert_eq!(reset.pixels, vec![0; 2 * 2 * 4]);
        assert_eq!(first.metrics().frames_decoded, 3);
        assert!(matches!(
            first.decode(&[]),
            Err(DecoderError::EmptyAccessUnit)
        ));
    }

    #[test]
    fn ffmpeg_decodes_frames_the_bridge_encoder_produced() {
        if !ffmpeg_available() {
            return;
        }
        let mut encoder =
            FfmpegEncoder::with_backend("ffmpeg", encoder_config(64, 64), EncoderBackend::Libx264)
                .unwrap();
        let first = encoder.encode(&frame(64, 64), false).unwrap();
        let codec_config = first.codec_config.clone().unwrap();
        let mut decoder = FfmpegDecoder::new(
            "ffmpeg",
            DecoderConfig {
                width: 64,
                height: 64,
                codec_config,
            },
        )
        .unwrap();

        let mut decoded = Vec::new();
        if let Some(frame) = decoder.decode(&first.annex_b).unwrap() {
            decoded.push(frame);
        }
        for _ in 0..11 {
            let encoded = encoder.encode(&frame(64, 64), false).unwrap();
            if let Some(frame) = decoder.decode(&encoded.annex_b).unwrap() {
                decoded.push(frame);
            }
        }
        assert!(
            decoded.len() >= 8,
            "expected the pipeline to prime and stay 1:1, got {} frames",
            decoded.len()
        );
        for frame in &decoded {
            assert_eq!((frame.width, frame.height), (64, 64));
            assert_eq!(frame.pixels.len(), 64 * 64 * 4);
        }
        assert_eq!(decoder.metrics().frames_decoded, decoded.len() as u64);
        assert_eq!(decoder.metrics().access_units_submitted, 12);
    }

    #[test]
    fn ffmpeg_reconfigure_follows_a_resized_stream() {
        if !ffmpeg_available() {
            return;
        }
        let mut encoder =
            FfmpegEncoder::with_backend("ffmpeg", encoder_config(64, 64), EncoderBackend::Libx264)
                .unwrap();
        let first = encoder.encode(&frame(64, 64), false).unwrap();
        let mut decoder = FfmpegDecoder::new(
            "ffmpeg",
            DecoderConfig {
                width: 64,
                height: 64,
                codec_config: first.codec_config.clone().unwrap(),
            },
        )
        .unwrap();
        drop(decoder.decode(&first.annex_b).unwrap());

        encoder.reconfigure(encoder_config(96, 64)).unwrap();
        let resized = encoder.encode(&frame(96, 64), false).unwrap();
        decoder
            .reconfigure(DecoderConfig {
                width: 96,
                height: 64,
                codec_config: resized.codec_config.clone().unwrap(),
            })
            .unwrap();
        assert_eq!(decoder.config().width, 96);

        let mut decoded = None;
        let mut access_unit = resized.annex_b;
        for _ in 0..12 {
            if let Some(frame) = decoder.decode(&access_unit).unwrap() {
                decoded = Some(frame);
                break;
            }
            access_unit = encoder.encode(&frame(96, 64), false).unwrap().annex_b;
        }
        let decoded = decoded.expect("resized stream produced no frame");
        assert_eq!((decoded.width, decoded.height), (96, 64));
        assert_eq!(decoded.pixels.len(), 96 * 64 * 4);
    }
}
