use std::env;
use std::time::Duration;

use anyhow::{Context, Result};
use navette_bridge::{Encoder, EncoderBackend, EncoderConfig, FfmpegEncoder, Frame};

fn main() -> Result<()> {
    let width = argument(1, 1920)?;
    let height = argument(2, 1080)?;
    let frames = argument(3, 60)?;
    let config = EncoderConfig {
        width,
        height,
        ..EncoderConfig::default()
    };
    let mut encoder = if env::args().nth(4).as_deref() == Some("software") {
        FfmpegEncoder::with_backend("ffmpeg", config, EncoderBackend::Libx264)?
    } else {
        FfmpegEncoder::new("ffmpeg", config)?
    };
    let mut timings = Vec::with_capacity(frames as usize);
    let mut bytes = 0_usize;
    let mut keyframes = 0_u32;
    let mut config_updates = 0_u32;
    for index in 0..frames {
        let encoded = encoder.encode(
            &synthetic_frame(width, height, index),
            index != 0 && index == frames / 2,
        )?;
        timings.push(encoded.encode_time);
        bytes = bytes.saturating_add(encoded.annex_b.len());
        keyframes += u32::from(encoded.keyframe);
        config_updates += u32::from(encoded.codec_config.is_some());
    }
    timings.sort_unstable();
    println!(
        "backend={} frames={} keyframes={} config_updates={} bytes={} p50_ms={:.3} p95_ms={:.3} max_ms={:.3}",
        encoder.backend().name(),
        frames,
        keyframes,
        config_updates,
        bytes,
        percentile(&timings, 50).as_secs_f64() * 1000.0,
        percentile(&timings, 95).as_secs_f64() * 1000.0,
        timings.last().copied().unwrap_or_default().as_secs_f64() * 1000.0,
    );
    Ok(())
}

fn argument(index: usize, default: u32) -> Result<u32> {
    env::args()
        .nth(index)
        .map(|value| value.parse::<u32>())
        .transpose()
        .with_context(|| format!("argument {index} must be an integer"))
        .map(|value| value.unwrap_or(default))
}

fn synthetic_frame(width: u32, height: u32, index: u32) -> Frame {
    let mut pixels = vec![0; width as usize * height as usize * 4];
    for pixel_index in 0..pixels.len() / 4 {
        let x = pixel_index as u32 % width;
        let y = pixel_index as u32 / width;
        let offset = pixel_index * 4;
        pixels[offset] = x.wrapping_add(index * 3) as u8;
        pixels[offset + 1] = y.wrapping_add(index * 2) as u8;
        pixels[offset + 2] = x.wrapping_add(y).wrapping_add(index) as u8;
        pixels[offset + 3] = 255;
    }
    Frame {
        width,
        height,
        pixels,
    }
}

fn percentile(values: &[Duration], percentile: usize) -> Duration {
    if values.is_empty() {
        return Duration::ZERO;
    }
    let index = (values.len() - 1) * percentile / 100;
    values[index]
}
