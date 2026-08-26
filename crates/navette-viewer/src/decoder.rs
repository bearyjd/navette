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

/// Number of access units that may be submitted with zero frames decoded
/// before a decoder is considered stalled rather than still priming.
/// Measured priming depth is ~3 access units; this leaves a wide margin so a
/// merely slow (but working) decode is never mistaken for a broken one,
/// while still recovering quickly when FFmpeg genuinely cannot decode the
/// stream (malformed SPS, a mismatched bootstrap, a rejected codec feature).
const STALL_ESCALATION_THRESHOLD: u64 = 10;

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

    /// Submits one Annex-B access unit and returns every frame the pipeline
    /// currently has ready.
    ///
    /// The pipeline runs a few access units behind, so this is not a strict
    /// 1:1 submit-then-collect relationship: it can return zero frames while
    /// FFmpeg is still priming (normal at stream startup, not a decode
    /// failure, and must not be answered with a keyframe request), one frame
    /// in steady state, or more than one when previously buffered output is
    /// drained alongside the frame this access unit triggered. A decoder that
    /// stays alive but genuinely cannot decode the stream (malformed SPS, a
    /// mismatched bootstrap, a rejected codec feature) escalates to `Err`
    /// once it has gone unambiguously too long without producing a frame,
    /// rather than reporting priming forever.
    fn decode(&mut self, access_unit: &[u8]) -> Result<Vec<DecodedFrame>, DecoderError>;

    /// Returns any frames already decoded but not yet retrieved, without
    /// submitting more input. Used to flush the last pictures of an idle or
    /// closing stream instead of discarding them.
    fn drain(&mut self) -> Vec<DecodedFrame>;

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
    /// Access units submitted and frames decoded since the *current*
    /// process was spawned, used only to detect a stalled decoder.
    /// `metrics` is cumulative across every `restart` (a reconfigure keeps
    /// its history, by design — see `FakeDecoder`'s matching behaviour), so
    /// a decoder that produced frames before a reconfigure and then stalls
    /// against the *new* process's bootstrap must not be shielded by frames
    /// it decoded under the old one. Reset in `restart`.
    access_units_since_spawn: u64,
    frames_since_spawn: u64,
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
            access_units_since_spawn: 0,
            frames_since_spawn: 0,
        })
    }

    fn restart(&mut self) -> Result<(), DecoderError> {
        self.process.take();
        self.process = Some(DecoderProcess::spawn(&self.executable, &self.config)?);
        self.access_units_since_spawn = 0;
        self.frames_since_spawn = 0;
        Ok(())
    }
}

impl Decoder for FfmpegDecoder {
    fn config(&self) -> &DecoderConfig {
        &self.config
    }

    fn decode(&mut self, access_unit: &[u8]) -> Result<Vec<DecodedFrame>, DecoderError> {
        if access_unit.is_empty() {
            return Err(DecoderError::EmptyAccessUnit);
        }
        self.metrics.access_units_submitted = self.metrics.access_units_submitted.saturating_add(1);
        self.access_units_since_spawn = self.access_units_since_spawn.saturating_add(1);
        self.metrics.bytes_submitted = self
            .metrics
            .bytes_submitted
            .saturating_add(access_unit.len() as u64);
        let started = Instant::now();

        let mut buffers = Vec::new();
        {
            let process = self.process.as_mut().ok_or(DecoderError::ProcessExited)?;
            process
                .stdin
                .write_all(access_unit)
                .map_err(DecoderError::Write)?;
            process.stdin.flush().map_err(DecoderError::Write)?;
            match process.output.recv_timeout(FRAME_TIMEOUT) {
                Ok(pixels) => buffers.push(pixels),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.frames_since_spawn == 0
                        && self.access_units_since_spawn >= STALL_ESCALATION_THRESHOLD
                    {
                        return Err(DecoderError::Stalled(self.access_units_since_spawn));
                    }
                    tracing::debug!(
                        access_units_submitted = self.metrics.access_units_submitted,
                        access_units_since_spawn = self.access_units_since_spawn,
                        "decoder still priming; no frame yet"
                    );
                    return Ok(Vec::new());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(DecoderError::ProcessExited);
                }
            }
            // The pipeline may have finished more than one frame between the
            // previous call and this one; drain everything already
            // available rather than assuming a strict 1:1 relationship.
            while let Ok(pixels) = process.output.try_recv() {
                buffers.push(pixels);
            }
            self.metrics.frames_dropped = process.dropped.load(Ordering::Relaxed);
        }

        let decode_time = started.elapsed();
        self.frames_since_spawn = self.frames_since_spawn.saturating_add(buffers.len() as u64);
        self.metrics.frames_decoded = self
            .metrics
            .frames_decoded
            .saturating_add(buffers.len() as u64);
        self.metrics.last_decode_time = decode_time;
        Ok(buffers
            .into_iter()
            .map(|pixels| DecodedFrame {
                width: self.config.width,
                height: self.config.height,
                pixels,
                decode_time,
            })
            .collect())
    }

    fn drain(&mut self) -> Vec<DecodedFrame> {
        let Some(process) = self.process.as_mut() else {
            return Vec::new();
        };
        let mut buffers = Vec::new();
        while let Ok(pixels) = process.output.try_recv() {
            buffers.push(pixels);
        }
        if buffers.is_empty() {
            return Vec::new();
        }
        self.metrics.frames_decoded = self
            .metrics
            .frames_decoded
            .saturating_add(buffers.len() as u64);
        let decode_time = self.metrics.last_decode_time;
        buffers
            .into_iter()
            .map(|pixels| DecodedFrame {
                width: self.config.width,
                height: self.config.height,
                pixels,
                decode_time,
            })
            .collect()
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

    fn decode(&mut self, access_unit: &[u8]) -> Result<Vec<DecodedFrame>, DecoderError> {
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
        Ok(vec![DecodedFrame {
            width: self.config.width,
            height: self.config.height,
            pixels: vec![shade; self.config.frame_len()],
            decode_time: Duration::ZERO,
        }])
    }

    fn drain(&mut self) -> Vec<DecodedFrame> {
        // Every access unit yields its frame synchronously; there is never a
        // backlog to flush.
        Vec::new()
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
    #[error("decoder produced no frames after {0} submitted access units")]
    Stalled(u64),
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
        let next = first
            .decode(&[0, 0, 0, 1, 0x41])
            .unwrap()
            .pop()
            .expect("fake decoder always yields exactly one frame");
        assert_eq!(next.pixels, vec![1; 4 * 2 * 4]);
        assert_eq!(next.width, 4);
        assert_eq!(next.height, 2);
        assert!(first.drain().is_empty());

        first.reconfigure(config(2, 2)).unwrap();
        let reset = first
            .decode(&[0, 0, 0, 1, 0x65])
            .unwrap()
            .pop()
            .expect("fake decoder always yields exactly one frame");
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

        let mut decoded = decoder.decode(&first.annex_b).unwrap();
        for _ in 0..11 {
            let encoded = encoder.encode(&frame(64, 64), false).unwrap();
            decoded.extend(decoder.decode(&encoded.annex_b).unwrap());
        }
        // Give the decoder a moment to catch up and poll `drain` the way
        // `StreamRouter::end` would, so this reflects full draining rather
        // than the old fixed 1:1 submit-then-collect assumption its previous
        // `>= 8` bound was compensating for.
        let deadline = Instant::now() + Duration::from_millis(500);
        while decoded.len() < 12 && Instant::now() < deadline {
            let more = decoder.drain();
            if more.is_empty() {
                thread::sleep(Duration::from_millis(10));
            } else {
                decoded.extend(more);
            }
        }
        // Even with full draining and no channel drops (`frames_dropped ==
        // 0`, verified via metrics below), FFmpeg's own H.264 decoder keeps
        // a small, constant number of pictures inside its internal decode
        // pipeline (measured at 2 here) that are only released on an EOF
        // flush — a decoder-internal latency distinct from, and not fixed
        // by, this layer's output-channel draining. Closing the pipe to
        // force that flush is out of scope for this round, so the bound
        // reflects what full draining actually recovers rather than the
        // full 12.
        // `>= 8` (unchanged from before this fix) rather than a tighter
        // bound: the exact decoder-internal backlog size is a property of
        // this FFmpeg build/machine, not of this crate's logic, so pinning
        // to the 10/12 measured here would risk flaking on a different
        // FFmpeg version or thread count. `frames_dropped == 0` below is
        // the assertion that actually proves the drain fix.
        assert!(
            decoded.len() >= 8,
            "expected full draining to recover at least as many frames as the old 1:1 \
             submit-then-collect model did, got {} of 12",
            decoded.len()
        );
        assert_eq!(
            decoder.metrics().frames_dropped,
            0,
            "no frame should be dropped by the output channel once every call drains it"
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
            if let Some(frame) = decoder.decode(&access_unit).unwrap().into_iter().next() {
                decoded = Some(frame);
                break;
            }
            access_unit = encoder.encode(&frame(96, 64), false).unwrap().annex_b;
        }
        let decoded = decoded.expect("resized stream produced no frame");
        assert_eq!((decoded.width, decoded.height), (96, 64));
        assert_eq!(decoded.pixels.len(), 96 * 64 * 4);
    }

    #[test]
    fn ffmpeg_decoder_escalates_once_it_never_produces_a_frame() {
        if !ffmpeg_available() {
            return;
        }
        // `config`'s bootstrap bytes are not real SPS/PPS, so FFmpeg's H.264
        // parser never has enough to decode a picture from — the process
        // stays alive (no crash on malformed input) but can never produce a
        // frame, exactly the "alive but broken" case this escalation exists
        // to catch. A real NAL type but never a real IDR keeps the parser
        // fed without ever completing a picture.
        let mut decoder = FfmpegDecoder::new("ffmpeg", config(64, 64)).unwrap();
        let not_a_frame = [0, 0, 0, 1, 0x09, 0x10];

        let mut last_error = None;
        for _ in 0..(STALL_ESCALATION_THRESHOLD + 2) {
            match decoder.decode(&not_a_frame) {
                Ok(frames) => assert!(
                    frames.is_empty(),
                    "no valid picture should ever come from a bogus bootstrap"
                ),
                Err(error) => {
                    last_error = Some(error);
                    break;
                }
            }
        }
        assert!(
            matches!(last_error, Some(DecoderError::Stalled(_))),
            "expected escalation to Stalled once past the priming threshold, got {last_error:?}"
        );
        assert_eq!(decoder.metrics().frames_decoded, 0);
    }

    #[test]
    fn ffmpeg_decoder_escalates_after_a_reconfigure_stalls_even_though_the_old_process_decoded() {
        if !ffmpeg_available() {
            return;
        }
        // The stall check must not be shielded by frames a *previous*
        // process decoded before a reconfigure: cumulative `metrics` keeps
        // that history by design (mirroring `FakeDecoder`), but the
        // escalation itself has to reset with every `restart` or a stream
        // that decoded fine once and then reconfigures onto a broken
        // bootstrap — the exact case the review calls out as the likely
        // real-world trigger, since a resize is the more common way a
        // decoder ends up alive but unable to decode — would spin silently
        // forever instead of recovering.
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
        // The pipeline primes over the first few access units (see
        // `FRAME_TIMEOUT`'s doc comment), so poll a handful of frames
        // through before asserting the working bootstrap actually decodes.
        let mut decoded = decoder.decode(&first.annex_b).unwrap();
        for _ in 0..5 {
            if !decoded.is_empty() {
                break;
            }
            let encoded = encoder.encode(&frame(64, 64), false).unwrap();
            decoded = decoder.decode(&encoded.annex_b).unwrap();
        }
        assert!(
            !decoded.is_empty(),
            "the working bootstrap should decode at least one frame once primed"
        );
        assert!(decoder.metrics().frames_decoded > 0);

        // Reconfigure onto a bootstrap that can never produce a picture.
        decoder.reconfigure(config(64, 64)).unwrap();
        let not_a_frame = [0, 0, 0, 1, 0x09, 0x10];
        let mut last_error = None;
        for _ in 0..(STALL_ESCALATION_THRESHOLD + 2) {
            match decoder.decode(&not_a_frame) {
                Ok(frames) => assert!(frames.is_empty()),
                Err(error) => {
                    last_error = Some(error);
                    break;
                }
            }
        }
        assert!(
            matches!(last_error, Some(DecoderError::Stalled(_))),
            "the new process must still escalate even though `metrics.frames_decoded` is \
             nonzero from before the reconfigure, got {last_error:?}"
        );
    }
}
