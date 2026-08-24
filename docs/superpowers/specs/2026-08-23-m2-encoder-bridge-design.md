# Navette M2 — Encoder Bridge Design

Status: Approved for implementation · Date: 2026-08-23

## Goal

Add a headless wprs client role that turns each session's Wayland surface
commits into bounded H.264 media streams, accepts scoped input and resize
events, and proves the complete path with a disposable Linux viewer. Android
remains M3.

## Upstream boundary

M2 pins wprs commit `57139a03abf466c5f24b737835a61121bd25c8c0`, the
revision exercised by M0. Stock `wprsd` remains an external per-session
process. `navette-bridge` links wprs serialization and protocol types at the
same revision and consumes the server's Unix socket directly.

The public wprs `Serializer::new_client` currently terminates the whole process
when its peer disconnects and uses an unbounded writer queue. That behavior is
safe for the standalone `wprsc` executable but not for an in-process daemon
component. A minimal, independently reviewed wprs change will expose a
recoverable client transport with bounded queues. Navette will pin the reviewed
commit. No rendering or protocol behavior in stock `wprsd` is changed.

## Bridge state

The bridge maintains a keyed scene model from wprs requests:

- `(client_id, surface_id)` identity; IDs from different Wayland clients never
  alias.
- buffer metadata and the most recent complete ARGB/XRGB image;
- damage, scale, transform, viewport, input region, and ordered children;
- toplevel and popup roles, titles, app IDs, parentage, and lifecycle;
- output metadata and cursor state.

An external wprs raw-buffer message is paired with exactly the following
surface commit. Invalid ordering, dimensions, stride, or buffer length rejects
the commit without mutating the last good scene. Limits are applied before
allocation.

Each toplevel produces a flattened BGRA frame. Subsurfaces and popups are
composited in protocol order with clipping and alpha. Damage is coalesced; the
encoder may encode a whole frame, but the bridge does not rebuild unchanged
pixels unnecessarily.

## Encoding

An `Encoder` trait isolates the bridge from the concrete backend. The initial
runtime backend drives the host FFmpeg executable with raw BGRA input and
Annex-B H.264 output:

1. Prefer Intel VA-API on the ThinkPad render node after a capability probe.
2. Fall back to `libx264` when hardware initialization fails.
3. Use low-latency settings: no B-frames, bounded lookahead, repeated headers,
   and an access-unit delimiter for deterministic framing.

The subprocess is in its own process group and is terminated on bridge exit or
reconfiguration. Raw-frame and encoded-output queues are bounded. When a
producer outruns a consumer, old non-key frames are dropped and the next
delivered frame is a fresh keyframe. Nothing buffers without a fixed limit.

## Media protocol

The existing JSON control endpoint remains `navette.v1`. A session media
endpoint is added at `/v1/sessions/{session}/media` with WebSocket subprotocol
`navette.media.v1`.

Binary server-to-client messages use a fixed network-endian header containing:

- magic and media protocol version;
- message kind (`stream_config`, `video`, `stream_end`, or `metrics`);
- flags (`keyframe`, `discontinuity`);
- stream ID, monotonically increasing sequence, and monotonic timestamp;
- payload length, coded width, and coded height.

The payload is codec configuration or one Annex-B access unit. Bounds are
validated before allocation. A new attachment receives configuration followed
by a keyframe.

Client-to-server commands remain small JSON text messages on the same media
socket: pointer motion/button/axis, keyboard key/modifiers, viewport resize,
and keyframe request. Unknown fields are additive; unknown commands, invalid
coordinates, oversized messages, or excessive rates receive structured errors.

## Input and resize safety

Media sockets are resolved to one registry session before the bridge accepts
events. The session name is never taken from an input payload, preventing
cross-session injection. Surface IDs must belong to the attached scene.
Coordinates are finite and clamped to the declared surface bounds; button and
key codes use explicit allowlisted ranges. Stuck keys/buttons are released on
disconnect.

Resize requests are limited to `320x240..3840x2160`, debounced, and rate
limited. They become wprs output/toplevel configure events only for the
attached session. Encoder reconfiguration creates a discontinuity and forces a
keyframe.

## Viewer and observability

`navette-viewer` is a disposable Linux validation client. It connects to one
media endpoint, decodes H.264 through FFmpeg, displays frames in one native
window, forwards pointer/keyboard events, and reports viewport changes. Its
decoder and window/input adapters are traits so protocol, decode, and input
tests run headlessly in CI.

The bridge exposes counters for captured, encoded, sent, and dropped frames;
queue depths; encode time; bytes; reconnects; and active backend. The viewer
can show a small performance HUD.

## M2 exit criteria

- Firefox and Claude Desktop both display, accept input, resize, detach, and
  reattach through the H.264 viewer.
- A 30-minute 1080p30 exercise has no unbounded queue or RSS growth.
- Capture-to-encoded-frame p95 is below 33 ms on the ThinkPad when VA-API is
  active; software fallback remains functional at a reduced rate.
- Slow consumers drop stale frames instead of growing memory.
- Reconnect supplies configuration and a keyframe within two seconds.
- Malformed and cross-session input tests pass.
- LAN/tailnet measurements are recorded when a second endpoint is available;
  their absence does not invalidate the local M2 implementation gate.

## Deferred

- Android MediaCodec/Compose integration and reconnect UX (M3)
- Application-layer authentication/PIN (roadmap Phase 1 hardening)
- Audio, image/file clipboard, and file transfer (M4)
- HEVC/AV1 negotiation and zero-copy DMA-BUF capture
