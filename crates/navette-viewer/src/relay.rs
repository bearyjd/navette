//! What to do with an input the connection refused.
//!
//! [`MediaClient::send_input`](crate::client::MediaClient::send_input) never
//! blocks, so under load it rejects rather than waits. What that costs depends
//! entirely on what was rejected, and the distinction is not "press versus
//! release" — it is **edge versus absolute state**:
//!
//! * An **edge** ([`MediaInput::KeyboardKey`], [`MediaInput::PointerButton`])
//!   describes a transition. Losing a release leaves the guest holding a key
//!   or button with nothing to correct it, so releases are retried.
//! * **Absolute state** ([`MediaInput::KeyboardModifiers`],
//!   [`MediaInput::ViewportResize`]) describes a whole condition. Losing one
//!   is worse than losing an edge: the producers short-circuit on equality
//!   (`native.rs`'s `modifier_changes`, `session.rs`'s `ResizeDebounce`), so
//!   nothing re-derives it and the guest stays wrong indefinitely — a guest
//!   stuck with Shift down uppercases everything the user types next.
//!
//!   Retrying these the way releases are retried would be a different bug: a
//!   state queued a tick ago can land after a newer one and clobber it. They
//!   are **coalesced** instead — only the newest value for each is kept, and
//!   that is what gets retried.
//! * Everything else ([`MediaInput::PointerMotion`], presses, axis) is a
//!   missed input and nothing more. Motion is re-sent continuously and
//!   self-corrects; a lost press costs one keystroke. These are dropped.
//!
//! This lives in the library rather than the binary so the policy is
//! unit-testable, which is how the two bugs described above were found.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use navette_protocol::media::MediaInput;

use crate::client::ClientError;

/// Ceiling on queued releases. Reaching it means the connection has been
/// refusing input for long enough that the session is in trouble; the bridge
/// releases a session's held keys and buttons when a client detaches, which is
/// the backstop for anything abandoned here.
pub const MAX_PENDING_RELEASES: usize = 128;

/// Age past which a queued release is abandoned. A release this old is being
/// delivered into a session that has moved on, and holding it forever is what
/// makes an unbounded queue harmful.
pub const RELEASE_STALE_AFTER: Duration = Duration::from_secs(5);

/// What losing this input to backpressure would cost.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Recovery {
    /// Must arrive eventually, and its exact value matters. Queued in order.
    Retry,
    /// Only the newest value matters. Superseded rather than queued.
    Coalesce(Slot),
    /// Costs one input; nothing is left inconsistent.
    Discard,
}

/// Which piece of absolute state an input carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Slot {
    Modifiers,
    Viewport,
}

fn recovery(input: &MediaInput) -> Recovery {
    match input {
        MediaInput::KeyboardKey { pressed: false, .. }
        | MediaInput::PointerButton { pressed: false, .. } => Recovery::Retry,
        MediaInput::KeyboardModifiers { .. } => Recovery::Coalesce(Slot::Modifiers),
        MediaInput::ViewportResize { .. } => Recovery::Coalesce(Slot::Viewport),
        _ => Recovery::Discard,
    }
}

/// What one [`InputRelay::dispatch`] did, for the caller to log.
#[derive(Debug, Default)]
pub struct RelayReport {
    /// How long each retried release had been waiting when it finally landed.
    pub redelivered: Vec<Duration>,
    /// Inputs whose loss is not worth recovering from, with why.
    pub discarded: Vec<(MediaInput, ClientError)>,
    /// Releases given up on: too old, or displaced by the queue cap.
    pub abandoned: Vec<MediaInput>,
}

/// Applies the module's policy to every input on its way to the connection.
#[derive(Debug, Default)]
pub struct InputRelay {
    /// Releases awaiting another attempt, oldest first.
    releases: VecDeque<(Instant, MediaInput)>,
    /// Newest unsent value for each piece of absolute state.
    modifiers: Option<MediaInput>,
    viewport: Option<MediaInput>,
}

impl InputRelay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Releases still waiting for another attempt.
    pub fn pending_releases(&self) -> usize {
        self.releases.len()
    }

    /// Whether any absolute state is still unsent.
    pub fn has_pending_state(&self) -> bool {
        self.modifiers.is_some() || self.viewport.is_some()
    }

    /// Sends `polled`, after first retrying whatever an earlier call could
    /// not get through. `send` is expected not to block; anything it rejects
    /// is handled per this module's policy.
    pub fn dispatch<F>(&mut self, polled: Vec<MediaInput>, now: Instant, mut send: F) -> RelayReport
    where
        F: FnMut(MediaInput) -> Result<(), ClientError>,
    {
        let mut report = RelayReport::default();
        self.expire(now, &mut report);

        // Retries first, so a release keeps its place ahead of newer input.
        // Absolute state carries no queue position: only the newest value is
        // ever held, and it is re-sent from scratch.
        let mut batch: Vec<(Option<Instant>, MediaInput)> = self
            .releases
            .drain(..)
            .map(|(queued_at, input)| (Some(queued_at), input))
            .collect();
        batch.extend(self.modifiers.take().map(|input| (None, input)));
        batch.extend(self.viewport.take().map(|input| (None, input)));
        batch.extend(polled.into_iter().map(|input| (None, input)));

        for (queued_at, input) in batch {
            match send(input.clone()) {
                Ok(()) => {
                    if let Some(queued_at) = queued_at {
                        report
                            .redelivered
                            .push(now.saturating_duration_since(queued_at));
                    }
                }
                Err(ClientError::InputBackpressure) => {
                    self.hold(input, queued_at.unwrap_or(now), &mut report);
                }
                Err(error) => report.discarded.push((input, error)),
            }
        }
        report
    }

    /// Files an input the connection refused, per its [`Recovery`].
    fn hold(&mut self, input: MediaInput, queued_at: Instant, report: &mut RelayReport) {
        match recovery(&input) {
            Recovery::Retry => {
                // The cap is a ceiling on damage, not a design goal: dropping
                // the oldest is the least-bad choice because it is the one
                // most likely to have already been superseded, and the
                // bridge's detach-time flush still covers what is lost.
                if self.releases.len() >= MAX_PENDING_RELEASES
                    && let Some((_, evicted)) = self.releases.pop_front()
                {
                    report.abandoned.push(evicted);
                }
                self.releases.push_back((queued_at, input));
            }
            // Newest wins: whatever was held is stale by definition.
            Recovery::Coalesce(Slot::Modifiers) => self.modifiers = Some(input),
            Recovery::Coalesce(Slot::Viewport) => self.viewport = Some(input),
            Recovery::Discard => report
                .discarded
                .push((input, ClientError::InputBackpressure)),
        }
    }

    fn expire(&mut self, now: Instant, report: &mut RelayReport) {
        while let Some((queued_at, _)) = self.releases.front() {
            if now.saturating_duration_since(*queued_at) < RELEASE_STALE_AFTER {
                break;
            }
            if let Some((_, input)) = self.releases.pop_front() {
                report.abandoned.push(input);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(keycode: u32, pressed: bool) -> MediaInput {
        MediaInput::KeyboardKey {
            client_id: 1,
            surface_id: 1,
            keycode,
            pressed,
        }
    }

    fn modifiers(shift: bool) -> MediaInput {
        MediaInput::KeyboardModifiers {
            client_id: 1,
            surface_id: 1,
            ctrl: false,
            alt: false,
            shift,
            caps_lock: false,
            logo: false,
            num_lock: false,
            layout_index: 0,
        }
    }

    fn viewport(width: u32) -> MediaInput {
        MediaInput::ViewportResize { width, height: 600 }
    }

    /// Rejects everything until `open`, then records what gets through.
    struct Sink {
        open: bool,
        sent: Vec<MediaInput>,
    }

    impl Sink {
        fn blocked() -> Self {
            Self {
                open: false,
                sent: Vec::new(),
            }
        }
        fn send(&mut self, input: MediaInput) -> Result<(), ClientError> {
            if self.open {
                self.sent.push(input);
                Ok(())
            } else {
                Err(ClientError::InputBackpressure)
            }
        }
    }

    #[test]
    fn a_refused_release_is_retried_until_it_lands() {
        let mut relay = InputRelay::new();
        let mut sink = Sink::blocked();
        let start = Instant::now();

        relay.dispatch(vec![key(30, false)], start, |i| sink.send(i));
        assert_eq!(relay.pending_releases(), 1, "release must be kept");
        assert!(sink.sent.is_empty());

        sink.open = true;
        let report = relay.dispatch(Vec::new(), start, |i| sink.send(i));
        assert_eq!(sink.sent, vec![key(30, false)]);
        assert_eq!(relay.pending_releases(), 0);
        assert_eq!(report.redelivered.len(), 1);
    }

    #[test]
    fn a_refused_press_is_discarded_rather_than_retried() {
        let mut relay = InputRelay::new();
        let mut sink = Sink::blocked();
        let report = relay.dispatch(vec![key(30, true)], Instant::now(), |i| sink.send(i));
        assert_eq!(relay.pending_releases(), 0);
        assert_eq!(report.discarded.len(), 1);
    }

    /// The bug this module exists for: modifiers are absolute state, and the
    /// producer never re-derives them, so dropping one leaves the guest stuck.
    #[test]
    fn refused_modifiers_are_kept_and_resent() {
        let mut relay = InputRelay::new();
        let mut sink = Sink::blocked();
        let now = Instant::now();

        relay.dispatch(vec![modifiers(false)], now, |i| sink.send(i));
        assert!(relay.has_pending_state(), "modifier state must be kept");

        sink.open = true;
        relay.dispatch(Vec::new(), now, |i| sink.send(i));
        assert_eq!(sink.sent, vec![modifiers(false)]);
        assert!(!relay.has_pending_state());
    }

    /// Coalescing, not queueing: a stale state must never land after a newer
    /// one. Queueing these would swap one latch for another.
    #[test]
    fn a_newer_state_supersedes_the_one_still_waiting() {
        let mut relay = InputRelay::new();
        let mut sink = Sink::blocked();
        let now = Instant::now();

        relay.dispatch(vec![modifiers(true)], now, |i| sink.send(i));
        relay.dispatch(vec![modifiers(false)], now, |i| sink.send(i));

        sink.open = true;
        relay.dispatch(Vec::new(), now, |i| sink.send(i));
        assert_eq!(
            sink.sent,
            vec![modifiers(false)],
            "only the newest state may be delivered"
        );
    }

    #[test]
    fn viewport_and_modifiers_are_held_independently() {
        let mut relay = InputRelay::new();
        let mut sink = Sink::blocked();
        let now = Instant::now();

        relay.dispatch(vec![modifiers(true), viewport(800)], now, |i| sink.send(i));
        sink.open = true;
        relay.dispatch(Vec::new(), now, |i| sink.send(i));

        assert!(sink.sent.contains(&modifiers(true)));
        assert!(sink.sent.contains(&viewport(800)));
    }

    #[test]
    fn queued_releases_keep_their_order_ahead_of_new_input() {
        let mut relay = InputRelay::new();
        let mut sink = Sink::blocked();
        let now = Instant::now();

        relay.dispatch(vec![key(30, false), key(31, false)], now, |i| sink.send(i));
        sink.open = true;
        relay.dispatch(vec![key(32, false)], now, |i| sink.send(i));

        assert_eq!(
            sink.sent,
            vec![key(30, false), key(31, false), key(32, false)]
        );
    }

    #[test]
    fn the_release_queue_is_capped() {
        let mut relay = InputRelay::new();
        let mut sink = Sink::blocked();
        let now = Instant::now();

        for keycode in 0..(MAX_PENDING_RELEASES as u32 + 10) {
            relay.dispatch(vec![key(keycode, false)], now, |i| sink.send(i));
        }
        assert_eq!(relay.pending_releases(), MAX_PENDING_RELEASES);
    }

    #[test]
    fn a_release_older_than_the_staleness_limit_is_abandoned() {
        let mut relay = InputRelay::new();
        let mut sink = Sink::blocked();
        let start = Instant::now();

        relay.dispatch(vec![key(30, false)], start, |i| sink.send(i));
        assert_eq!(relay.pending_releases(), 1);

        let later = start + RELEASE_STALE_AFTER + Duration::from_millis(1);
        let report = relay.dispatch(Vec::new(), later, |i| sink.send(i));
        assert_eq!(relay.pending_releases(), 0);
        assert_eq!(report.abandoned.len(), 1);
    }

    /// A non-backpressure failure is terminal for that input: retrying an
    /// invalid event would just fail forever.
    #[test]
    fn a_rejected_release_is_not_queued() {
        let mut relay = InputRelay::new();
        let report = relay.dispatch(vec![key(30, false)], Instant::now(), |_| {
            Err(ClientError::Disconnected)
        });
        assert_eq!(relay.pending_releases(), 0);
        assert_eq!(report.discarded.len(), 1);
    }

    /// Guards the classification itself: a new absolute-state variant added to
    /// the protocol must be classified deliberately, not fall into `Discard`
    /// and latch the guest the way modifiers did.
    #[test]
    fn every_variant_is_classified_deliberately() {
        assert_eq!(recovery(&key(30, false)), Recovery::Retry);
        assert_eq!(recovery(&key(30, true)), Recovery::Discard);
        assert_eq!(
            recovery(&modifiers(true)),
            Recovery::Coalesce(Slot::Modifiers)
        );
        assert_eq!(recovery(&viewport(800)), Recovery::Coalesce(Slot::Viewport));
        assert_eq!(
            recovery(&MediaInput::PointerButton {
                client_id: 1,
                surface_id: 1,
                button: 0x110,
                pressed: false,
            }),
            Recovery::Retry
        );
        assert_eq!(
            recovery(&MediaInput::PointerMotion {
                client_id: 1,
                surface_id: 1,
                x: 0.0,
                y: 0.0,
            }),
            Recovery::Discard
        );
        assert_eq!(recovery(&MediaInput::RequestKeyframe), Recovery::Discard);
    }
}
