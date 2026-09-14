# Image clipboard over session-scoped bulk blobs

## Goal

Let a phone and a guest exchange PNG, JPEG, and WebP clipboard images while
preserving the existing bidirectional text clipboard behavior. Establish a
bounded, authenticated blob transport that a later file-transfer feature can
reuse.

## Approach

- Add per-session, runtime-directory-backed blob storage with opaque IDs,
  atomic writes, strict MIME/ID validation, streaming limits, and cleanup.
- Add authenticated `POST` and `GET` blob routes to the existing guarded API
  router; media messages carry descriptors only, never bytes.
- Extend the media protocol and clipboard state machine with a blob payload
  while keeping that state machine free of filesystem I/O.
- Have the bridge perform guest blob I/O and satisfy guest paste requests;
  missing or unsatisfiable blobs produce an empty transfer.
- Add Android upload/download coordination: only announce a completed upload,
  discard superseded fetch/upload results, and place fetched images into the
  Android clipboard without replaying them after reconnect.

## TDD sequence

1. Rust/Kotlin protocol validation fixtures for blob IDs and image MIME types.
2. Pure clipboard-state tests for text precedence, blob echo suppression, and
   empty transfer on missing blobs.
3. Blob-store tests for atomicity, streamed limits, malformed IDs, budget,
   cancellation cleanup, and replacement safety.
4. Authenticated router tests for successful round trips and 404/413/415/507.
5. Bridge tests for guest image copy/paste and text regression coverage.
6. Android coordinator tests for upload/fetch races and echo behavior.
7. Device validation for phone-to-guest, guest-to-phone, and reconnect cases.

## Acceptance criteria

- Blob bytes use authenticated HTTP only; the media WebSocket transports only
  `{ id, mime, size }` descriptors.
- Production limits are 64 MiB per blob and 256 MiB per session.
- Blob paths are session-scoped opaque IDs; no client filename or path reaches
  the filesystem.
- Text remains preferred when a guest offers both text and image data.
- A newer clipboard blob supersedes a pending older operation without a stale
  Android clipboard update or guest announcement.
- Existing text clipboard behavior and reconnect semantics remain unchanged.

## Out of scope

File-transfer UI or CLI commands, filenames, delete/range/resume support,
image transcoding, HTML/primary selections, drag-and-drop, audio, and clipboard
replay after reconnect.

## Risks

Android clipboard URI permissions and cache lifetime need device verification.
All router tests must prove the new endpoints inherit bearer authentication and
browser-origin rejection. The blob store must account for partial uploads in
its session budget and clean a cancelled upload's partial file.
