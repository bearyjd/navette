//! Client-side performance HUD.
//!
//! Every number here is reconstructed from what the viewer already receives on
//! the media socket — frame arrivals, packet sizes, packet sequence numbers
//! and the decoder's own timing. Nothing on the wire is added for it, and the
//! computation is deliberately free of any window or drawing concern so it can
//! be asserted against a scripted clock in a headless test.

use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};

use navette_protocol::media::MediaKind;

use crate::router::StreamPacket;

/// Rolling window every rate in a [`HudSample`] is measured over.
pub const HUD_WINDOW: Duration = Duration::from_secs(1);

/// How many of a stream's first packets only establish the sequence baseline
/// instead of being audited for gaps.
///
/// Attaching replays up to two packets per stream — the stream's latest
/// configuration and its latest keyframe — from before this client connected,
/// at the sequence numbers they were originally published with. Live traffic
/// then resumes from wherever the stream has actually got to, so the jump
/// from the replayed keyframe to the first live packet is history this client
/// was never sent, not a drop. Three packets covers that replay plus the first
/// live one; the cost is that a genuine drop inside a stream's opening
/// packets goes uncounted, which is a far better trade than reporting a
/// fictitious one on every attach.
const BASELINE_PACKETS: u64 = 3;

/// One stream's performance figures at a point in time.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HudSample {
    /// Decoded frames per second over the rolling window.
    pub fps: f64,
    /// Video bits per second over the rolling window.
    pub bitrate_bps: f64,
    /// How long the decoder took over its most recent access unit.
    pub decode_time: Duration,
    /// Packets the server never delivered, counted from gaps in the stream's
    /// sequence numbering. The media hub drops packets for a client that
    /// falls behind, so a rising count means this viewer is the slow one.
    pub dropped_packets: u64,
    /// Packets the bridge flagged as following an encoder discontinuity, e.g.
    /// after a resize forced a fresh keyframe.
    pub discontinuities: u64,
}

impl fmt::Display for HudSample {
    /// A compact single-line rendering, short enough to overlay on a small
    /// window and to read in a log.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "FPS {:.1} KBPS {:.0} DEC {:.1}MS DROP {} DISC {}",
            self.fps,
            self.bitrate_bps / 1000.0,
            self.decode_time.as_secs_f64() * 1000.0,
            self.dropped_packets,
            self.discontinuities
        )
    }
}

/// Accumulates one stream's HUD inputs.
///
/// Time is always supplied by the caller rather than read from the clock, so
/// the rolling-window arithmetic is exercised deterministically in tests.
#[derive(Debug, Default)]
pub struct StreamHud {
    frames: VecDeque<Instant>,
    video_bytes: VecDeque<(Instant, usize)>,
    last_sequence: Option<u64>,
    packets: u64,
    dropped_packets: u64,
    discontinuities: u64,
    decode_time: Duration,
}

impl StreamHud {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that one decoded picture reached the viewer.
    pub fn record_frame(&mut self, now: Instant) {
        self.frames.push_back(now);
        trim(&mut self.frames, now, |at| *at);
    }

    /// Records one packet's wire cost, sequence position and decoder timing.
    pub fn record_packet(&mut self, now: Instant, packet: &StreamPacket) {
        self.note_sequence(packet.sequence);
        if packet.discontinuity {
            self.discontinuities = self.discontinuities.saturating_add(1);
        }
        if let Some(decoder) = &packet.decoder {
            self.decode_time = decoder.last_decode_time;
        }
        if packet.kind == MediaKind::Video {
            self.video_bytes.push_back((now, packet.wire_bytes));
            trim(&mut self.video_bytes, now, |(at, _)| *at);
        }
    }

    /// Counts packets the server never delivered.
    ///
    /// A stream's sequence numbering covers every kind of packet, not just
    /// video, so gaps are tracked across all of them. A sequence that does not
    /// advance is the hub replaying an older packet and is neither a drop nor
    /// a reason to rewind the baseline; a stream's opening packets only
    /// establish that baseline, for the reason [`BASELINE_PACKETS`] gives.
    fn note_sequence(&mut self, sequence: u64) {
        let observed = self.packets;
        self.packets = self.packets.saturating_add(1);
        let Some(last) = self.last_sequence else {
            self.last_sequence = Some(sequence);
            return;
        };
        if sequence <= last {
            return;
        }
        if observed >= BASELINE_PACKETS {
            self.dropped_packets = self.dropped_packets.saturating_add(sequence - last - 1);
        }
        self.last_sequence = Some(sequence);
    }

    /// Computes the current figures, discarding anything that has aged out of
    /// the rolling window.
    pub fn sample(&mut self, now: Instant) -> HudSample {
        trim(&mut self.frames, now, |at| *at);
        trim(&mut self.video_bytes, now, |(at, _)| *at);
        let video_bits = self
            .video_bytes
            .iter()
            .map(|(_, bytes)| *bytes as f64 * 8.0)
            .sum::<f64>();
        HudSample {
            fps: rate(self.frames.len() as f64, self.frames.front().copied(), now),
            bitrate_bps: rate(video_bits, self.video_bytes.front().map(|(at, _)| *at), now),
            decode_time: self.decode_time,
            dropped_packets: self.dropped_packets,
            discontinuities: self.discontinuities,
        }
    }
}

/// Drops entries that fell out of the rolling window. Entries are appended in
/// arrival order, so only the front has to be examined.
fn trim<T>(entries: &mut VecDeque<T>, now: Instant, at: impl Fn(&T) -> Instant) {
    while entries
        .front()
        .is_some_and(|entry| now.saturating_duration_since(at(entry)) > HUD_WINDOW)
    {
        entries.pop_front();
    }
}

/// Spreads `total` over the span from the oldest retained entry to `now`.
///
/// Measuring against `now` rather than the newest entry is what makes this a
/// live meter: when a stream goes idle the span keeps growing and the rate
/// decays toward zero, instead of freezing at whatever it was when the last
/// frame landed.
fn rate(total: f64, oldest: Option<Instant>, now: Instant) -> f64 {
    let Some(oldest) = oldest else {
        return 0.0;
    };
    let span = now.saturating_duration_since(oldest).as_secs_f64();
    if span <= 0.0 {
        return 0.0;
    }
    total / span
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::DecoderMetrics;

    fn packet(kind: MediaKind, sequence: u64, wire_bytes: usize) -> StreamPacket {
        StreamPacket {
            stream_id: 1,
            kind,
            sequence,
            wire_bytes,
            discontinuity: false,
            decoder: None,
        }
    }

    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    #[test]
    fn frame_cadence_becomes_frames_per_second_over_the_rolling_window() {
        let base = Instant::now();
        let mut hud = StreamHud::new();
        // Ten frames 102ms apart, sampled 1.02s after the first one. The
        // oldest has just aged out of the one-second window, leaving nine
        // frames spanning the 918ms back to the sample point.
        for index in 0..10 {
            hud.record_frame(at(base, index * 102));
        }
        let sample = hud.sample(at(base, 1020));
        assert!(
            (sample.fps - 9.80).abs() < 0.01,
            "expected ~9.8 fps, got {}",
            sample.fps
        );
    }

    #[test]
    fn an_idle_stream_decays_toward_zero_instead_of_freezing() {
        // A stream whose window stops changing simply stops producing frames.
        // The HUD must show that as a falling frame rate rather than holding
        // the last rate it measured, which is what dividing by the span back
        // to `now` (not to the newest frame) buys.
        let base = Instant::now();
        let mut hud = StreamHud::new();
        for index in 0..30 {
            hud.record_frame(at(base, index * 33));
        }
        let live = hud.sample(at(base, 990)).fps;
        assert!((live - 30.0).abs() < 1.0, "expected ~30 fps, got {live}");

        // Half a second with nothing new: the frames still inside the window
        // now span half again as long, so the rate has to have fallen.
        let idle = hud.sample(at(base, 1490)).fps;
        assert!(
            idle < live * 0.75,
            "an idle stream should decay: {idle} vs {live}"
        );
        // ...and once nothing is left in the window at all, it reads zero.
        assert_eq!(hud.sample(at(base, 3000)).fps, 0.0);
    }

    #[test]
    fn only_video_packets_count_toward_bitrate() {
        let base = Instant::now();
        let mut hud = StreamHud::new();
        hud.record_packet(at(base, 0), &packet(MediaKind::StreamConfig, 1, 1_000));
        hud.record_packet(at(base, 0), &packet(MediaKind::Video, 2, 12_500));
        hud.record_packet(at(base, 500), &packet(MediaKind::Video, 3, 12_500));

        // 25_000 video bytes over the 1s span back to the oldest retained
        // packet; the 1_000-byte configuration packet is not video.
        let sample = hud.sample(at(base, 1000));
        assert!(
            (sample.bitrate_bps - 200_000.0).abs() < 1.0,
            "expected 200kbps, got {}",
            sample.bitrate_bps
        );
        assert_eq!(sample.dropped_packets, 0);
    }

    #[test]
    fn sequence_gaps_are_counted_but_attaching_mid_stream_is_not_a_drop() {
        let base = Instant::now();
        let mut hud = StreamHud::new();
        // Attaching mid-stream: the hub replays the stream's latest
        // configuration and keyframe at their original sequences, then live
        // packets resume from wherever the stream has actually got to. The
        // six-packet jump from the replayed keyframe to the first live packet
        // is history this client was never sent, not a drop.
        hud.record_packet(at(base, 0), &packet(MediaKind::StreamConfig, 5, 100));
        hud.record_packet(at(base, 0), &packet(MediaKind::Video, 6, 100));
        hud.record_packet(at(base, 10), &packet(MediaKind::Video, 12, 100));
        assert_eq!(hud.sample(at(base, 10)).dropped_packets, 0);

        hud.record_packet(at(base, 20), &packet(MediaKind::Video, 13, 100));
        assert_eq!(hud.sample(at(base, 20)).dropped_packets, 0);

        // Now a real gap: the hub evicted 14 and 15 for a slow client.
        hud.record_packet(at(base, 30), &packet(MediaKind::Video, 16, 100));
        assert_eq!(hud.sample(at(base, 30)).dropped_packets, 2);

        // A late duplicate of an already-seen packet is not a drop either,
        // and must not make the next packet look like one.
        hud.record_packet(at(base, 40), &packet(MediaKind::Video, 13, 100));
        hud.record_packet(at(base, 50), &packet(MediaKind::Video, 17, 100));
        assert_eq!(hud.sample(at(base, 50)).dropped_packets, 2);
    }

    #[test]
    fn the_baseline_window_does_not_swallow_drops_on_a_stream_that_starts_fresh() {
        // A stream that begins after this client attached is replayed
        // nothing, so its numbering starts at 1 and every later gap is real.
        let base = Instant::now();
        let mut hud = StreamHud::new();
        for sequence in 1..=BASELINE_PACKETS {
            hud.record_packet(at(base, sequence), &packet(MediaKind::Video, sequence, 100));
        }
        assert_eq!(hud.sample(at(base, 10)).dropped_packets, 0);

        hud.record_packet(at(base, 20), &packet(MediaKind::Video, 8, 100));
        assert_eq!(
            hud.sample(at(base, 20)).dropped_packets,
            8 - BASELINE_PACKETS - 1
        );
    }

    #[test]
    fn decode_time_and_discontinuities_come_from_the_packets_that_report_them() {
        let base = Instant::now();
        let mut hud = StreamHud::new();
        assert_eq!(hud.sample(base).decode_time, Duration::ZERO);

        hud.record_packet(
            at(base, 0),
            &StreamPacket {
                discontinuity: true,
                decoder: Some(DecoderMetrics {
                    last_decode_time: Duration::from_millis(4),
                    ..DecoderMetrics::default()
                }),
                ..packet(MediaKind::Video, 1, 100)
            },
        );
        let sample = hud.sample(at(base, 10));
        assert_eq!(sample.decode_time, Duration::from_millis(4));
        assert_eq!(sample.discontinuities, 1);

        // A packet with no live decoder (the stream was just torn down) must
        // not erase the last real measurement.
        hud.record_packet(at(base, 20), &packet(MediaKind::Video, 2, 100));
        assert_eq!(
            hud.sample(at(base, 20)).decode_time,
            Duration::from_millis(4)
        );
    }

    #[test]
    fn a_fresh_hud_reports_zeroes_rather_than_dividing_by_an_empty_window() {
        let base = Instant::now();
        let mut hud = StreamHud::new();
        assert_eq!(hud.sample(base), HudSample::default());

        // A single frame at the sample instant leaves a zero-length span; the
        // rate must be reported as zero rather than as infinity or NaN.
        hud.record_frame(base);
        let sample = hud.sample(base);
        assert_eq!(sample.fps, 0.0);
        assert!(sample.fps.is_finite());
    }

    #[test]
    fn the_display_line_is_compact_and_carries_every_figure() {
        let sample = HudSample {
            fps: 29.94,
            bitrate_bps: 2_500_000.0,
            decode_time: Duration::from_micros(4200),
            dropped_packets: 3,
            discontinuities: 1,
        };
        assert_eq!(
            sample.to_string(),
            "FPS 29.9 KBPS 2500 DEC 4.2MS DROP 3 DISC 1"
        );
    }
}
