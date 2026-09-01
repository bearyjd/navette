//! Throwaway harness guest, not part of the shipped viewer: opens N
//! always-on Wayland windows and repaints each as fast as `minifb` allows, no
//! throttling. Used to measure `navetted`'s bridge-loop throughput under a
//! multi-window commit burst, without needing the full sway+viewer+wtype
//! stack -- see docs/HANDOFF.md, "Decode was the ceiling" section.
//!
//! Env vars: `PAINTLOOP_WINDOWS` (default 1), `PAINTLOOP_SECONDS` (default 15).
//! Run under a session's `WAYLAND_DISPLAY` (navetted sets this when it spawns
//! an app), not directly against a host compositor.

use std::env;
use std::time::{Duration, Instant};

use minifb::{Window, WindowOptions};

const WIDTH: usize = 1280;
const HEIGHT: usize = 720;

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let windows = env_usize("PAINTLOOP_WINDOWS", 1);
    let seconds = env_usize("PAINTLOOP_SECONDS", 15);

    let mut opened: Vec<Window> = (0..windows)
        .map(|index| {
            let mut window = Window::new(
                &format!("paintloop-{index}"),
                WIDTH,
                HEIGHT,
                WindowOptions {
                    resize: false,
                    ..WindowOptions::default()
                },
            )
            .expect("failed to open paintloop window");
            // No cadence of our own -- the point is to commit as fast as the
            // scene graph will accept, to force several commits into one
            // bridge-loop iteration.
            window.set_target_fps(0);
            window
        })
        .collect();

    let mut buffers: Vec<Vec<u32>> = (0..windows).map(|_| vec![0u32; WIDTH * HEIGHT]).collect();
    let deadline = Instant::now() + Duration::from_secs(seconds as u64);
    let mut frame: u32 = 0;

    while Instant::now() < deadline {
        let mut any_open = false;
        for (index, window) in opened.iter_mut().enumerate() {
            if !window.is_open() {
                continue;
            }
            any_open = true;
            let shade = frame.wrapping_add(index as u32 * 40) % 256;
            let color = (shade << 16) | (shade << 8) | shade;
            buffers[index].fill(color);
            let _ = window.update_with_buffer(&buffers[index], WIDTH, HEIGHT);
        }
        if !any_open {
            break;
        }
        frame = frame.wrapping_add(1);
    }
}
