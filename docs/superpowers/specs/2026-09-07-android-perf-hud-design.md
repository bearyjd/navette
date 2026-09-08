# Navette — Android Performance HUD Design

Closes the `performance HUD` item in `docs/ROADMAP.md`'s Phase 1 ("Phone
attach — the demo"). Companion to the Linux viewer's HUD
(`crates/navette-viewer/src/hud.rs`), which this deliberately mirrors where
the two clients can measure the same quantity, and deliberately does not
where they cannot.

## Goal

Show, on the phone, what the media stream is actually doing: frame rate,
bitrate, decode cost, frame age, network round trip, and the two fault
counters the wire already carries. Off by default; long-press to toggle.

Two reasons this is the next slice rather than a nice-to-have. The roadmap
argues it directly (§4.2: "HUD early — every performance-camp product treats
metrics as a tuning prerequisite, not a luxury"). And every latency defect in
this project's history was found by measuring — the 626ms poll cycles, the
FFmpeg-spawn stall, the repeat-burst length metric. The phone-over-tailnet
link is the one hop that has never been instrumented from the client side.

## Metrics

| Field | Meaning | Source |
|---|---|---|
| `FPS` | Decoded pictures per second, 1s rolling window | decoder output callbacks |
| `KBPS` | Video kilobits per second, 1s rolling window | `MediaHeader.payload_len` on `Video` packets |
| `DEC` | Feed-to-output latency of the most recent access unit | `feed()` → `onOutputBufferAvailable` |
| `AGE` | Now minus the newest frame's arrival | decoder output callbacks |
| `RTT` | Application round trip to `navetted`'s media socket | ping/pong, below |
| `DROP` | Packets the server never delivered | gaps in `MediaHeader.sequence` |
| `DISC` | Packets flagged after an encoder discontinuity | `MediaFlags.discontinuity` |

`FPS`, `KBPS`, `DROP` and `DISC` are computed exactly as `hud.rs` computes
them, including the one-second rolling window (`HUD_WINDOW`) and the
three-packet sequence baseline (`BASELINE_PACKETS`) that keeps the attach
replay — a stream's last config plus last keyframe, republished at their
original sequence numbers — from being counted as a drop. Those four columns
are directly comparable between the desktop viewer and the phone.

**`DEC` is not comparable, and the spec says so rather than implying it is.**
The Rust viewer measures the decoder's own time over one access unit. On
Android, `MediaCodec` renders straight to the `SurfaceView` and the only
observable interval is `feed()` to `onOutputBufferAvailable`, which includes
queueing inside the codec. It is a useful number; it is a different number.

`AGE` exists to cover `RTT`'s blind spot — see below.

## Round-trip time

`MediaInput::Ping { nonce: u64 }` client-to-server, answered with
`MediaServerMessage::Pong { nonce: u64 }`. The client stamps the send, matches
the nonce on return, and reports the delta. One ping per second.

**Answered by the media socket task**, not the bridge loop. That is where the
mechanism already exists: `crates/navetted/src/api.rs:152-210` parses client
JSON inline and is the sole writer of server-to-client JSON, while
`MediaAttachment::recv()` yields `Arc<MediaPacket>` and nothing else. Routing
a pong through the bridge loop would need either a new `MediaKind` control
packet or a JSON side-channel out of the hub — a materially larger change,
and a different metric.

**Stated limitation.** A socket-task pong stays low while the bridge loop
stalls. That is precisely the failure mode this project has hit twice: the
~600ms FFmpeg spawn on the bridge loop, and the tokio driver block that froze
every timer in the process. During either, `RTT` would read "network fine".

`AGE` is the mitigation, and it costs no protocol work: if the bridge stops
producing frames, frame age climbs regardless of why. `RTT` low with `AGE`
climbing is the signature of a server-side stall; both climbing is the
network. Reading them together is the point of showing both.

## Protocol compatibility

No `MEDIA_VERSION` bump and no capability negotiation. Both directions
degrade safely, by existing behaviour rather than by new code:

- **New client, old `navetted`:** the unknown `ping` tag fails
  `serde_json::from_str::<MediaInput>` and the server replies
  `{"type":"error","code":"invalid_input"}` — an error *response*, not a
  disconnect (`api.rs:181-209`). The Android client logs server text at warn
  and explicitly does not tear the connection down
  (`MediaClient.kt:354-361`); an unparseable body falls to `getOrNull()`.
  Net effect: a warn line per ping, connection healthy, `RTT` blank.
- **New `navetted`, old client:** never sends a ping, never receives a pong.

`MediaInput::Ping` passes `validate()` for any nonce — there is no invalid
value. The Rust viewer neither sends pings nor handles `Pong`; that is a
deliberate boundary, not an unfinished half of this work.

## Rate limiting and degradation

The media socket allows `MAX_INPUT_MESSAGES_PER_SECOND = 240` (`api.rs:28`);
a 1 Hz ping is 0.4% of that budget, including during a drag. If a ping is
nonetheless rate-limited, lost, or unanswered, the HUD shows no `RTT` sample
for that second. It must never show an error state, a stale value presented
as live, or an unbounded reading. A nonce that returns after its successor
has been sent is discarded, not attributed to the wrong send.

## Android structure

`ui/session/SessionScreen.kt` is 827 lines, already past this project's
800-line ceiling, so the HUD is not added to it:

- **`ui/session/SessionHud.kt`** — the metric arithmetic. Time is supplied by
  the caller, never read from the clock, exactly as `hud.rs` does it, so the
  rolling windows are asserted against a scripted clock in plain Kotlin unit
  tests with no Android dependency. Knows nothing about Compose.
- **`ui/session/SessionHudOverlay.kt`** — the composable, beside
  `SessionOverlay.kt`. Renders a `HudSample`; computes nothing.
- **`SessionController` / `MediaClient`** — wiring only: feed packet
  arrivals, decoder callbacks and pong receipts into the accumulator, and
  drive the 1 Hz ping.

## Toggle

Off by default. Long-press on the video toggles it — confirmed unclaimed:
no long-press handling exists in `GestureInterpreter.kt` or
`SessionScreen.kt`. Two-finger tap remains right-click, single tap remains
left-click, and pinch remains zoom; long-press must not fire during a pinch
or drag. Visible in release builds, because the interesting numbers come from
a real link, not a debug one.

The toggle state is hoisted to `SessionScreen` and keyed the same way
`imeRaised` and `ViewTransformHolder` are, so it survives a reconnect
rebuild instead of resetting whenever the link flaps.

## Testing

- **Kotlin unit tests** against a scripted clock: window arithmetic, the
  sequence-gap baseline, nonce matching including an out-of-order return, and
  degradation to a blank `RTT` when no pong arrives.
- **Rust unit tests**: serde round trip and `validate()` for `Ping`, serde for
  `Pong`, and an `api.rs` test that a ping is answered with the matching
  nonce.
- **On-device**: the Pixel 10 Pro Fold, which carries the merged M3 build.
  Confirm the HUD toggles, that `FPS`/`KBPS` track a repainting guest, and
  that dropping wifi drives `AGE` up while the reconnect runs.

## Exit criteria

1. HUD toggles by long-press, off by default, surviving a reconnect.
2. All seven fields populate against a live session over the tailnet.
3. `DROP` reads zero across an ordinary attach — the baseline rule works.
4. Old-server compatibility observed, not just reasoned: a new client against
   an unpatched `navetted` keeps its connection and blanks `RTT`.
5. Android unit tests and the Rust suite green; all three CI checks green.

## Deferred

- Bridge-loop responsiveness as its own measured number (approach B above).
- RTT in the Linux viewer.
- Any HUD history, graphing, or logging to file — this shows current values.
