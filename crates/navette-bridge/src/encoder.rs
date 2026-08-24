use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, poll};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use thiserror::Error;

use crate::Frame;

const OUTPUT_QUEUE_CAPACITY: usize = 4;
const OUTPUT_IDLE_MS: u16 = 5;
const MAX_ENCODED_ACCESS_UNIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EncoderBackend {
    Vaapi { device: PathBuf },
    Libx264,
    Fake,
}

impl EncoderBackend {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Vaapi { .. } => "h264_vaapi",
            Self::Libx264 => "libx264",
            Self::Fake => "fake",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub frame_rate: u32,
    pub bitrate: u32,
    pub keyframe_interval: u32,
}

impl EncoderConfig {
    pub fn validate(self) -> Result<Self, EncoderError> {
        if self.width < 2
            || self.height < 2
            || self.width > 8192
            || self.height > 8192
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
            || !(1..=240).contains(&self.frame_rate)
            || !(100_000..=100_000_000).contains(&self.bitrate)
            || self.keyframe_interval == 0
        {
            return Err(EncoderError::InvalidConfig);
        }
        Ok(self)
    }
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            frame_rate: 30,
            bitrate: 6_000_000,
            keyframe_interval: 60,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedFrame {
    pub annex_b: Vec<u8>,
    pub codec_config: Option<Vec<u8>>,
    pub keyframe: bool,
    pub encode_time: Duration,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EncoderMetrics {
    pub frames_submitted: u64,
    pub frames_encoded: u64,
    pub frames_dropped: u64,
    pub bytes_encoded: u64,
    pub last_encode_time: Duration,
}

pub trait Encoder {
    fn backend(&self) -> &EncoderBackend;
    fn config(&self) -> EncoderConfig;
    fn encode(&mut self, frame: &Frame, force_keyframe: bool)
    -> Result<EncodedFrame, EncoderError>;
    fn reconfigure(&mut self, config: EncoderConfig) -> Result<(), EncoderError>;
    fn metrics(&self) -> EncoderMetrics;
}

pub struct FfmpegEncoder {
    executable: PathBuf,
    backend: EncoderBackend,
    config: EncoderConfig,
    process: Option<EncoderProcess>,
    metrics: EncoderMetrics,
}

pub struct FakeEncoder {
    config: EncoderConfig,
    sequence: u64,
    metrics: EncoderMetrics,
    backend: EncoderBackend,
}

impl FakeEncoder {
    pub fn new(config: EncoderConfig) -> Result<Self, EncoderError> {
        Ok(Self {
            config: config.validate()?,
            sequence: 0,
            metrics: EncoderMetrics::default(),
            backend: EncoderBackend::Fake,
        })
    }
}

impl Encoder for FakeEncoder {
    fn backend(&self) -> &EncoderBackend {
        &self.backend
    }

    fn config(&self) -> EncoderConfig {
        self.config
    }

    fn encode(
        &mut self,
        frame: &Frame,
        force_keyframe: bool,
    ) -> Result<EncodedFrame, EncoderError> {
        self.metrics.frames_submitted = self.metrics.frames_submitted.saturating_add(1);
        if frame.width != self.config.width || frame.height != self.config.height {
            return Err(EncoderError::FrameDimensions);
        }
        if frame.pixels.len() != frame.width as usize * frame.height as usize * 4 {
            return Err(EncoderError::FrameLength);
        }
        let keyframe = force_keyframe
            || self
                .sequence
                .is_multiple_of(u64::from(self.config.keyframe_interval));
        let nal_type = if keyframe { 0x65 } else { 0x41 };
        let mut annex_b = vec![0, 0, 0, 1, nal_type];
        annex_b.extend_from_slice(&self.sequence.to_be_bytes());
        self.sequence = self.sequence.saturating_add(1);
        self.metrics.frames_encoded = self.metrics.frames_encoded.saturating_add(1);
        self.metrics.bytes_encoded = self
            .metrics
            .bytes_encoded
            .saturating_add(annex_b.len() as u64);
        Ok(EncodedFrame {
            annex_b,
            codec_config: keyframe.then(|| vec![0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 1]),
            keyframe,
            encode_time: Duration::ZERO,
        })
    }

    fn reconfigure(&mut self, config: EncoderConfig) -> Result<(), EncoderError> {
        self.config = config.validate()?;
        self.sequence = 0;
        Ok(())
    }

    fn metrics(&self) -> EncoderMetrics {
        self.metrics.clone()
    }
}

impl FfmpegEncoder {
    pub fn new(
        executable: impl Into<PathBuf>,
        config: EncoderConfig,
    ) -> Result<Self, EncoderError> {
        let executable = executable.into();
        let config = config.validate()?;
        let backend = select_backend(&executable, config);
        Self::with_backend(executable, config, backend)
    }

    pub fn with_backend(
        executable: impl Into<PathBuf>,
        config: EncoderConfig,
        backend: EncoderBackend,
    ) -> Result<Self, EncoderError> {
        let executable = executable.into();
        let config = config.validate()?;
        let process = EncoderProcess::spawn(&executable, &backend, config)?;
        Ok(Self {
            executable,
            backend,
            config,
            process: Some(process),
            metrics: EncoderMetrics::default(),
        })
    }

    fn restart(&mut self) -> Result<(), EncoderError> {
        self.process.take();
        self.process = Some(EncoderProcess::spawn(
            &self.executable,
            &self.backend,
            self.config,
        )?);
        Ok(())
    }
}

impl Encoder for FfmpegEncoder {
    fn backend(&self) -> &EncoderBackend {
        &self.backend
    }

    fn config(&self) -> EncoderConfig {
        self.config
    }

    fn encode(
        &mut self,
        frame: &Frame,
        force_keyframe: bool,
    ) -> Result<EncodedFrame, EncoderError> {
        self.metrics.frames_submitted = self.metrics.frames_submitted.saturating_add(1);
        if frame.width != self.config.width || frame.height != self.config.height {
            return Err(EncoderError::FrameDimensions);
        }
        let expected = frame.width as usize * frame.height as usize * 4;
        if frame.pixels.len() != expected {
            return Err(EncoderError::FrameLength);
        }
        if force_keyframe {
            self.restart()?;
        }
        let started = Instant::now();
        let process = self.process.as_mut().ok_or(EncoderError::ProcessExited)?;
        process
            .stdin
            .write_all(&frame.pixels)
            .map_err(EncoderError::Write)?;
        process.stdin.flush().map_err(EncoderError::Write)?;
        let access_unit = process
            .output
            .recv_timeout(Duration::from_secs(3))
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => EncoderError::OutputTimeout,
                mpsc::RecvTimeoutError::Disconnected => EncoderError::ProcessExited,
            })?;
        let encode_time = started.elapsed();
        let (keyframe, codec_config) = inspect_annex_b(&access_unit);
        self.metrics.frames_encoded = self.metrics.frames_encoded.saturating_add(1);
        self.metrics.frames_dropped = process.dropped.load(Ordering::Relaxed);
        self.metrics.bytes_encoded = self
            .metrics
            .bytes_encoded
            .saturating_add(access_unit.len() as u64);
        self.metrics.last_encode_time = encode_time;
        Ok(EncodedFrame {
            annex_b: access_unit,
            codec_config,
            keyframe,
            encode_time,
        })
    }

    fn reconfigure(&mut self, config: EncoderConfig) -> Result<(), EncoderError> {
        self.config = config.validate()?;
        self.restart()
    }

    fn metrics(&self) -> EncoderMetrics {
        self.metrics.clone()
    }
}

struct EncoderProcess {
    child: Child,
    stdin: ChildStdin,
    output: Receiver<Vec<u8>>,
    dropped: Arc<AtomicU64>,
    reader: Option<JoinHandle<()>>,
}

impl EncoderProcess {
    fn spawn(
        executable: &Path,
        backend: &EncoderBackend,
        config: EncoderConfig,
    ) -> Result<Self, EncoderError> {
        let mut command = ffmpeg_command(executable, backend, config);
        let mut child = command.spawn().map_err(EncoderError::Spawn)?;
        let stdin = child.stdin.take().ok_or(EncoderError::MissingPipe)?;
        let stdout = child.stdout.take().ok_or(EncoderError::MissingPipe)?;
        let (sender, output) = mpsc::sync_channel(OUTPUT_QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let reader_dropped = Arc::clone(&dropped);
        let reader = thread::spawn(move || read_access_units(stdout, sender, reader_dropped));
        Ok(Self {
            child,
            stdin,
            output,
            dropped,
            reader: Some(reader),
        })
    }
}

impl Drop for EncoderProcess {
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

fn ffmpeg_command(executable: &Path, backend: &EncoderBackend, config: EncoderConfig) -> Command {
    let mut command = Command::new(executable);
    command.args(["-hide_banner", "-loglevel", "error"]);
    if let EncoderBackend::Vaapi { device } = backend {
        command.args(["-vaapi_device", &device.to_string_lossy()]);
    }
    command
        .args([
            "-f",
            "rawvideo",
            "-pixel_format",
            "bgra",
            "-video_size",
            &format!("{}x{}", config.width, config.height),
            "-framerate",
            &config.frame_rate.to_string(),
            "-i",
            "pipe:0",
            "-an",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    match backend {
        EncoderBackend::Vaapi { .. } => {
            command.args([
                "-vf",
                "format=nv12,hwupload",
                "-c:v",
                "h264_vaapi",
                "-async_depth",
                "1",
                "-bf",
                "0",
                "-g",
                &config.keyframe_interval.to_string(),
                "-b:v",
                &config.bitrate.to_string(),
                "-aud",
                "1",
            ]);
        }
        EncoderBackend::Libx264 => {
            command.args([
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-bf",
                "0",
                "-g",
                &config.keyframe_interval.to_string(),
                "-b:v",
                &config.bitrate.to_string(),
                "-x264-params",
                "scenecut=0:repeat-headers=1:aud=1",
            ]);
        }
        EncoderBackend::Fake => unreachable!("fake backend does not spawn FFmpeg"),
    }
    command.args(["-flush_packets", "1", "-f", "h264", "pipe:1"]);
    command
}

fn select_backend(executable: &Path, config: EncoderConfig) -> EncoderBackend {
    ["/dev/dri/renderD128", "/dev/dri/renderD129"]
        .into_iter()
        .map(PathBuf::from)
        .find(|device| probe_vaapi(executable, device, config))
        .map(|device| EncoderBackend::Vaapi { device })
        .unwrap_or(EncoderBackend::Libx264)
}

fn probe_vaapi(executable: &Path, device: &Path, config: EncoderConfig) -> bool {
    if !device.exists() {
        return false;
    }
    let mut child = match Command::new(executable)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-vaapi_device",
            &device.to_string_lossy(),
            "-f",
            "lavfi",
            "-i",
            "color=size=64x64:rate=1",
            "-frames:v",
            "1",
            "-vf",
            "format=nv12,hwupload",
            "-c:v",
            "h264_vaapi",
            "-b:v",
            &config.bitrate.to_string(),
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                drop(child.kill());
                drop(child.wait());
                return false;
            }
        }
    }
}

fn read_access_units(
    mut stdout: std::process::ChildStdout,
    sender: SyncSender<Vec<u8>>,
    dropped: Arc<AtomicU64>,
) {
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let ready = {
            let mut descriptors = [PollFd::new(stdout.as_fd(), PollFlags::POLLIN)];
            poll(&mut descriptors, 1000_u16)
        };
        let Ok(ready) = ready else {
            break;
        };
        if ready == 0 {
            continue;
        }
        let mut access_unit = Vec::new();
        let mut oversized = false;
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => return,
                Ok(length)
                    if access_unit.len().saturating_add(length) <= MAX_ENCODED_ACCESS_UNIT =>
                {
                    access_unit.extend_from_slice(&buffer[..length]);
                }
                Ok(_) => oversized = true,
                Err(_) => return,
            }
            // Both configured encoders emit an AUD and flush once per frame.
            // A short idle boundary avoids waiting for the following frame while
            // still allowing large access units to span many pipe reads.
            let more = {
                let mut descriptors = [PollFd::new(stdout.as_fd(), PollFlags::POLLIN)];
                poll(&mut descriptors, OUTPUT_IDLE_MS)
            };
            if !matches!(more, Ok(value) if value > 0) {
                break;
            }
        }
        if oversized {
            dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        for unit in split_on_aud(access_unit) {
            if sender.try_send(unit).is_err() {
                dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn split_on_aud(bytes: Vec<u8>) -> Vec<Vec<u8>> {
    let starts = nal_ranges(&bytes);
    let aud_offsets = starts
        .iter()
        .filter_map(|(start, header, _)| (bytes[*header] & 0x1f == 9).then_some(*start))
        .collect::<Vec<_>>();
    if aud_offsets.len() <= 1 {
        return (!bytes.is_empty()).then_some(bytes).into_iter().collect();
    }
    let mut output = Vec::with_capacity(aud_offsets.len());
    if aud_offsets[0] > 0 {
        output.push(bytes[..aud_offsets[0]].to_vec());
    }
    for (index, start) in aud_offsets.iter().enumerate() {
        let end = aud_offsets.get(index + 1).copied().unwrap_or(bytes.len());
        output.push(bytes[*start..end].to_vec());
    }
    output
}

fn inspect_annex_b(bytes: &[u8]) -> (bool, Option<Vec<u8>>) {
    let mut keyframe = false;
    let mut config = Vec::new();
    for (start, header, end) in nal_ranges(bytes) {
        match bytes[header] & 0x1f {
            5 => keyframe = true,
            7 | 8 => config.extend_from_slice(&bytes[start..end]),
            _ => {}
        }
    }
    (keyframe, (!config.is_empty()).then_some(config))
}

fn nal_ranges(bytes: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 < bytes.len() {
        let start_code = if bytes[index..].starts_with(&[0, 0, 0, 1]) {
            Some(4)
        } else if bytes[index..].starts_with(&[0, 0, 1]) {
            Some(3)
        } else {
            None
        };
        if let Some(length) = start_code {
            starts.push((index, index + length));
            index += length;
        } else {
            index += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, (start, header))| {
            let end = starts
                .get(index + 1)
                .map(|(next, _)| *next)
                .unwrap_or(bytes.len());
            (*start, *header, end)
        })
        .collect()
}

pub struct FrameQueue {
    frames: VecDeque<Frame>,
    capacity: usize,
    dropped: u64,
}

impl FrameQueue {
    pub fn new(capacity: usize) -> Self {
        assert!((1..=256).contains(&capacity));
        Self {
            frames: VecDeque::with_capacity(capacity),
            capacity,
            dropped: 0,
        }
    }

    pub fn push(&mut self, frame: Frame) {
        if self.frames.len() == self.capacity {
            self.frames.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.frames.push_back(frame);
    }

    pub fn pop(&mut self) -> Option<Frame> {
        self.frames.pop_front()
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

#[derive(Debug, Error)]
pub enum EncoderError {
    #[error("invalid encoder configuration")]
    InvalidConfig,
    #[error("frame dimensions do not match encoder configuration")]
    FrameDimensions,
    #[error("frame pixel length is invalid")]
    FrameLength,
    #[error("failed to spawn FFmpeg: {0}")]
    Spawn(std::io::Error),
    #[error("FFmpeg pipe is unavailable")]
    MissingPipe,
    #[error("failed to write FFmpeg input: {0}")]
    Write(std::io::Error),
    #[error("FFmpeg did not emit a frame before the deadline")]
    OutputTimeout,
    #[error("FFmpeg exited")]
    ProcessExited,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32) -> Frame {
        Frame {
            width,
            height,
            pixels: vec![0; width as usize * height as usize * 4],
        }
    }

    #[test]
    fn annex_b_parser_extracts_configuration_and_keyframe() {
        let bytes = vec![
            0, 0, 0, 1, 9, 0xf0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 4,
        ];
        let (keyframe, config) = inspect_annex_b(&bytes);
        assert!(keyframe);
        assert_eq!(config.unwrap(), [0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3]);
    }

    #[test]
    fn access_units_split_at_aud_nals() {
        let bytes = vec![0, 0, 1, 9, 0, 0, 1, 0x65, 1, 0, 0, 1, 9, 0, 0, 1, 0x41, 2];
        let units = split_on_aud(bytes);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0], [0, 0, 1, 9, 0, 0, 1, 0x65, 1]);
        assert_eq!(units[1], [0, 0, 1, 9, 0, 0, 1, 0x41, 2]);
    }

    #[test]
    fn frame_queue_drops_oldest_frame_at_capacity() {
        let mut queue = FrameQueue::new(2);
        queue.push(frame(2, 2));
        queue.push(frame(4, 2));
        queue.push(frame(6, 2));
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.dropped(), 1);
        assert_eq!(queue.pop().unwrap().width, 4);
    }

    #[test]
    fn fake_encoder_is_deterministic_and_forces_keyframes() {
        let config = EncoderConfig {
            width: 2,
            height: 2,
            frame_rate: 30,
            bitrate: 500_000,
            keyframe_interval: 60,
        };
        let mut first = FakeEncoder::new(config).unwrap();
        let mut second = FakeEncoder::new(config).unwrap();
        let first_frame = first.encode(&frame(2, 2), false).unwrap();
        let second_frame = second.encode(&frame(2, 2), false).unwrap();
        assert_eq!(first_frame, second_frame);
        assert!(first.encode(&frame(2, 2), true).unwrap().keyframe);
    }

    #[test]
    fn software_ffmpeg_emits_decodable_annex_b() {
        if Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            return;
        }
        let config = EncoderConfig {
            width: 64,
            height: 64,
            frame_rate: 30,
            bitrate: 500_000,
            keyframe_interval: 30,
        };
        let mut encoder =
            FfmpegEncoder::with_backend("ffmpeg", config, EncoderBackend::Libx264).unwrap();
        let encoded = encoder.encode(&frame(64, 64), false).unwrap();
        assert!(encoded.keyframe);
        assert!(encoded.codec_config.is_some());
        assert!(!encoded.annex_b.is_empty());
        let mut decoder = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "h264",
                "-i",
                "pipe:0",
                "-frames:v",
                "1",
                "-f",
                "null",
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        decoder
            .stdin
            .take()
            .unwrap()
            .write_all(&encoded.annex_b)
            .unwrap();
        assert!(decoder.wait().unwrap().success());

        assert!(!encoder.encode(&frame(64, 64), false).unwrap().keyframe);
        assert!(encoder.encode(&frame(64, 64), true).unwrap().keyframe);
        encoder
            .reconfigure(EncoderConfig {
                width: 66,
                ..config
            })
            .unwrap();
        assert!(encoder.encode(&frame(66, 64), false).unwrap().keyframe);
    }
}
