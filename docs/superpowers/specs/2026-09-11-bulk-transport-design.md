# Bulk Byte Transport — Design

**Status:** approved, not yet implemented
**Supersedes nothing. Extends:** `docs/superpowers/specs/2026-09-08-clipboard-sync-design.md`

## Why this exists

Image clipboard looked like a small extension of text clipboard sync. It is not.
`MAX_INPUT_MESSAGE` is 16 KiB (`crates/navette-protocol/src/media.rs:10`), and the
WebSocket read path tears the media socket down above 32 KiB rather than refusing
gracefully. An image structurally cannot ride `MediaInput::SetClipboard`. The
clipboard *state machine* is reusable; the transport underneath it does not exist.

Once that was clear, a second fact followed: image clipboard and file transfer are
the same problem wearing different clothes. Both need to move a bounded blob of
bytes between phone and guest, and neither can use the control path. So the
transport is designed once, here, for both.

## Decisions taken before design (recorded, not re-litigated)

1. **One transport for both consumers** — image clipboard and file transfer.
2. **Session-scoped blob store only.** No filesystem paths on the wire, ever. The
   daemon API has zero authentication and the tailnet is the entire security
   boundary; a path-taking endpoint would hand a tailnet peer arbitrary host read
   and write. This constraint is the reason the design looks the way it does.
3. **64 MB per blob, streamed to disk.** Covers clipboard images plus ordinary
   documents and photos. Streaming keeps peak memory flat regardless of blob size,
   which keeps the store viable on tmpfs and so preserves the free cleanup in §2.
4. **Single-slot clipboard, queued files.** The clipboard holds exactly one blob;
   a new image copy replaces and unlinks the previous one. File transfers queue
   against a per-session budget.

## Orientation facts this design rests on

Each of these was verified in the tree, not assumed.

- **The guest can see host directories.** `RealProcessRunner::spawn`
  (`crates/navetted/src/supervisor.rs:38-40`) calls `.args().envs().process_group(0)`
  — `envs` is additive, there is no `env_clear()`, and `current_dir` is never set.
  The guest inherits navetted's full environment and runs as the same uid.
- **A per-session private directory already exists.**
  `$XDG_RUNTIME_DIR/navette/<session>/`, created 0700 by `create_private_directory`
  (`supervisor.rs:322-331`), currently holding `wprs.sock`.
- **Its cleanup already exists.** `supervisor.rs:247` does
  `fs::remove_dir_all(runtime_root.join(name))` on unregister.
- **The daemon has no body-carrying HTTP route today.** `crates/navetted/src/api.rs:77-80`
  registers exactly three routes: `/healthz`, `/v1/ws`, and the per-session media
  WebSocket.
- **`GuestEvent::TransferFromGuest { bytes: Vec<u8> }` is already bytes.** Only the
  `String::from_utf8` at `crates/navetted/src/clipboard.rs:107` makes the guest side
  text-only. The guest direction needs almost nothing.
- **Unknown `MediaKind` throws on Android** (`MediaProtocol.kt:192`); unknown JSON
  text is logged harmlessly. Server-to-client additions must therefore be text.

## 1. Scope

This spec covers the bulk transport plus its first consumer, image clipboard, end
to end.

File transfer is the second consumer the transport is shaped for. Its endpoints
need no change when it lands; §9 records what it will add. Its user-facing surface
gets its own spec.

## 2. Where bytes live

```
$XDG_RUNTIME_DIR/navette/<session>/     (exists, 0700)
  wprs.sock                             (exists)
  blobs/                                (new, 0700)
    <32-hex-id>                         complete blob, 0600
    <32-hex-id>.meta                    JSON sidecar: {"mime","size"}, 0600
    <32-hex-id>.part                    in-flight upload, 0600
```

**Blob metadata lives in a sidecar file, not in memory.** `GET` must answer with the
blob's `Content-Type`, so the daemon has to remember each blob's mime. An in-memory
map would be the obvious choice and the wrong one: sessions are registry-persisted
and `wprsd` is a separate process, so a session outlives a navetted restart while
its blobs sit on disk. A `<id>.meta` sidecar written before the `.part` rename
survives that restart; an in-memory map would strand every existing blob as
un-serveable. The sidecar is written first, the rename is the commit point, and a
blob with no readable sidecar is treated as absent (404) and unlinked.

No new lifetime, no new cleanup path, no sweeper task. The session directory is
already deleted wholesale on unregister, and `XDG_RUNTIME_DIR` is tmpfs, so a
crashed daemon's orphans are collected by the OS at logout or reboot. That is the
entire garbage-collection story, and it is why §"Decisions" chose a size envelope
that keeps the store on tmpfs.

The cost being accepted: tmpfs is RAM. §6's limits are what make that safe, and
they are not optional.

**Filenames never become path components.** A blob's storage name is its opaque
server-generated id and nothing else. When file transfer needs a human-readable
name it will carry one as JSON metadata and materialize it inside a *server-named*
directory — never by joining a client-supplied string onto a path.

## 3. Endpoints

Two routes alongside the three that exist, on the same router.

```
POST /v1/sessions/{session}/blobs
  Content-Type: the blob's mime
  body: raw bytes, streamed
  → 201 {"id": "<32 hex>", "mime": "...", "size": N}
  → 404 no such session
  → 413 over the per-blob cap
  → 415 absent or malformed Content-Type
  → 507 session blob budget exhausted

GET /v1/sessions/{session}/blobs/{id}
  → 200 streamed body, with Content-Type and Content-Length
  → 404 no such session, malformed id, or no such blob
```

Upload streams `axum::body::Body` into `<id>.part` through `tokio::io::copy`,
counting bytes as they arrive, then renames to `<id>`. Peak memory is one buffer at
any blob size. `DefaultBodyLimit` must be disabled on this route specifically —
its 2 MB default would otherwise reject every blob — and replaced by the streaming
counter in §6, which is the only limit that cannot be lied to.

`GET` returns 404 for a malformed id and for an absent one alike, so the route
discloses nothing about what exists.

**The upload route does not enforce the clipboard mime allowlist.** It accepts any
well-formed mime token up to 255 bytes, because the same route serves file transfer,
where `application/pdf` and friends are entirely valid. The clipboard allowlist in
§4 is enforced where clipboard semantics actually apply — at `SetClipboardBlob`
validation and in the guest-selection preference order of §5 — never at upload time.
A blob whose mime the clipboard will not accept is still a legal blob.

There is no `DELETE`. The clipboard's single slot deletes by replacement; file
transfer will add one when it has a consumer that needs it.

## 4. Protocol additions

Both directions are small JSON on the existing media socket. No new `MediaKind`, so
nothing throws on an old Android client.

```rust
// phone → daemon, in MediaInput
SetClipboardBlob { id: String, mime: String }

// daemon → phone, in MediaServerMessage
ClipboardBlob { id: String, mime: String, size: u64 }
```

**The invariant, stated once for both sides of the pair:** bytes move only over
HTTP; the media socket carries only announcements. The daemon announces and the
phone fetches; the phone uploads and then announces. Neither side ever receives
bulk bytes it did not ask for.

`MediaInput::validate()` gains two rejections, both applied before either value
reaches code that constructs a path:

```rust
InputValidationError::InvalidBlobId          // id !~ ^[0-9a-f]{32}$
InputValidationError::UnsupportedClipboardMime  // mime outside the allowlist
```

Clipboard mime allowlist: `image/png`, `image/jpeg`, `image/webp`. Nothing else is
accepted for a clipboard blob, in either direction.

### Fetch failure and supersession

Splitting announcement from fetch creates a window, and the window will be hit
routinely — most often by the single-slot replacement itself. The rules:

- **Announcement supersedes.** The phone tracks only the most recently announced
  blob id. If a `ClipboardBlob` arrives while an earlier fetch is in flight, that
  earlier fetch is abandoned and its result discarded *even if it succeeds*.
- **A failed fetch of a superseded blob is silent.** It is the expected outcome of
  a race the design creates deliberately, not an error, and it reaches no user.
- **A failed fetch of the current blob drops that update and nothing more.** The
  clipboard simply does not gain the image. **There is no retry queue.** A parked
  retry firing after newer content has already arrived is an open defect on master
  in the text path; this design declines to grow a second one. The next
  announcement is the recovery mechanism.

This makes the unlink race correct by rule rather than by accident. Guest copies A,
the daemon announces A, the phone begins fetching; the guest copies B, `bridge.rs`
commits B and unlinks A. The in-flight `GET` of A either completes (the handler
already holds the fd, and POSIX keeps the inode alive) or returns 404 (it had not
opened yet). The phone discards A's result in both cases, because B's announcement
superseded it. The outcome does not depend on which way the race lands — which is
the point, since relying on the fd-holding path would be depending on an accident.

**Across a reconnect:** an announcement whose fetch never completed is lost, and
nothing is replayed on attach. That is consistent with the deliberate no-replay
choice in the text clipboard design. The observable consequence, which §8 asserts:
after a reconnect the phone does *not* hold the image, and a fresh copy on the
guest delivers normally.

## 5. ClipboardSync changes

```rust
pub enum ClipboardPayload {
    Text(String),
    Blob { id: BlobId, mime: String, size: u64 },
}
```

`phone_text: Option<String>` becomes `phone_payload: Option<ClipboardPayload>`. The
echo tokens and the one-shot logic keep their shape; they compare payloads where
they compared strings.

**`clipboard.rs` stays pure.** No I/O, no clock, no async, no wprs types. This is
what makes every decision in it a plain unit test, and it survives the change
intact: `SyncAction::AnswerGuest` carries the payload, and for a blob the payload
carries the **id only**. `bridge.rs::apply_sync_action` performs the file read.
A reviewer should treat any I/O appearing in `clipboard.rs` as a defect.

Offer shape follows the payload:

- `Text` offers the existing five mime types in `OFFERED_MIME_TYPES`.
- `Blob { mime }` offers exactly that one mime. **The daemon never transcodes.** If
  a guest wants `image/png` and the phone sent `image/jpeg`, png is simply not
  offered.

**A guest may still request a mime the payload cannot satisfy** — a badly behaved
client, or a request racing a payload that changed between the offer and the pull.
`PasteRequested` was deliberately built with no early return so that `AnswerGuest`
is structurally guaranteed, and that property must not be weakened here: an
unsatisfiable request answers with an **empty transfer**, never with silence.
Silence is what hangs a guest waiting on a pull, and an unanswered pull is already
an open hazard on master. The same answer covers a blob whose file has gone missing
underneath us. Adding a way to not answer would add a second door into that class
of hang; this design closes it by construction instead.

Guest → phone gains a preference order, applied to the guest's
`SourceMetadata.mime_types`:

1. Any mime in `OFFERED_MIME_TYPES` → request it, handle as text (unchanged path).
2. Otherwise any allowlisted image mime → request it, write the bytes to a blob,
   emit `ClipboardBlob`.
3. Otherwise → ignore the selection.

Text wins over image deliberately. A browser selection commonly offers both, and
text is what is actually useful on a phone.

## 6. Limits

**Both directions are bounded, and they are bounded differently because the bytes
arrive differently.** Stating the cap only in HTTP terms would leave the guest path
unlimited — the same one-side-of-the-pair mistake that produced the last branch's
missed P1.

- **Phone → daemon: 64 MB** (`MAX_BLOB_BYTES`), enforced by counting streamed
  bytes. A lying `Content-Length` changes nothing; on overflow the write aborts,
  the `.part` file is unlinked, and the response is 413.
- **Guest → daemon: the same 64 MB**, checked against `bytes.len()` in
  `bridge.rs::handle_guest_data` *before* any blob is written, **and against the
  same session budget below.** Both bounds apply in both directions; giving the
  guest only the per-blob cap would repeat the one-sided mistake one level down.
  An over-cap or over-budget guest transfer writes nothing and is not announced to
  the phone; the guest's selection is simply not propagated.

  This bound limits what navette *stores*. How much wprs allocates while reading a
  message is out of scope for this spec — see the tracked follow-up in
  `docs/HANDOFF.md`.
- **Per session: 256 MB** across all blobs, clipboard and file alike — one budget,
  no per-kind carve-out. `POST` returns 507 when full. The budget is computed from
  the blobs present on disk, not from a counter that could drift, and **`.part`
  files count toward it**. Without that, N concurrent uploads each see room and
  overshoot by 64N MB. Two concurrent POSTs can still each admit themselves before
  either commits, so the budget is a bound with one blob of slack per concurrent
  upload, not an exact ceiling. That overshoot is bounded and accepted; an exact
  ceiling would need a lock across the admission check and the reservation, which
  is not worth it at these sizes.
- **Clipboard holds one blob.** A new image replaces and unlinks the previous one,
  so clipboard storage is capped at one blob however long the session runs. The
  unlink happens in `bridge.rs::apply_sync_action` when the new payload is
  committed, not in `clipboard.rs` — the decision layer stays free of I/O (§5), so
  it names the blob to drop and the bridge removes it. Unlinking on commit rather
  than on announcement matters: a guest mid-paste against the old blob must not
  have the file vanish under it, so the previous blob is dropped only once the new
  payload has replaced it in `phone_payload`.
- **A dropped upload unlinks its own `.part`** through a Drop guard, so tokio
  cancelling the handler mid-stream does not leak. Session teardown catches
  anything a hard kill leaves behind.

## 7. Security

**Status update: the remedy this section used to point at now exists and applies to
these routes automatically.** `docs/superpowers/specs/2026-09-12-hardening-design.md`
landed `Origin` rejection and an API-wide bearer token on the daemon's router, with
no loopback exemption and no per-route opt-in. Because §3 places the blob routes on
that same router (`crates/navetted/src/api.rs`), they inherit both guards the moment
they're registered — the same way `/healthz` and the media socket did. There is
nothing to wire for these routes specifically: no route on this router is reachable
without a valid `Authorization: Bearer <token>` header, and none is reachable at all
from a request carrying an `Origin` header.

That makes authentication, not equivalence, the reason this surface is safe to add,
and the equivalence argument below is retained only as context for why the surface
was judged acceptable *before* that branch existed — it is no longer what carries
this section.

*(Retained for context, no longer load-bearing.)* Before API-wide auth landed, the
argument here was that these routes do not *widen* the then-unauthenticated
boundary: a peer that could reach `POST /v1/sessions/{s}/blobs` could already attach
to `/v1/sessions/{s}/media`, read the entire screen, and inject input — total
compromise of every session — so blob upload and download added nothing such a peer
did not already have. That reasoning was sound for its purpose (justifying new
routes on an unauthenticated daemon) but never claimed to make the daemon itself
safe, which is exactly the gap the hardening branch closed.

The properties below are real and worth having, but they are defense in depth
*within* the authenticated boundary — not the reason the new surface is acceptable.

- **Ids are 128 CSPRNG bits, hex-encoded, generated server-side.** Never
  client-chosen, never sanitized — validated against `^[0-9a-f]{32}$` by whitelist
  *before* any path is constructed. This is load-bearing: a client-supplied `{id}`
  on the GET route would be a directory-traversal primitive, which would undo the
  exact reason session-scoped storage was chosen over filesystem paths.
- **The session segment resolves through the registry**, as the existing media
  route already does. It is never joined onto a path directly.
- **The 64 MB cap and 256 MB budget bound the disk-fill primitive** that an
  unauthenticated POST would otherwise be. Both are enforced during streaming.
- **The daemon never decodes image bytes.** It is a pipe. No image parser runs on
  data arriving from an unauthenticated endpoint — an entire CVE class declined
  rather than mitigated.
- **Blob bytes are never logged at any level, in either language.** This is the
  same rule clipboard text already carries, now covering a second data type. Ids,
  sizes, and mimes may be logged.
- `.part` and completed blobs are created 0600 inside the 0700 session directory.

## 8. Testing

- `clipboard.rs` keeps pure, payload-agnostic decision tests. No I/O appears in
  them because none appears in the module.
- A new `blobs.rs` module gets: id generation, id validation as a property test
  (no string outside the whitelist ever yields a path), cap enforcement against a
  *lying* `Content-Length`, budget accounting, and `.part` cleanup under handler
  cancellation.
- HTTP-level tests cover 404, 413, 415, and 507.
- Android proves the one-shot echo semantics for blobs **and** for text explicitly,
  both sides of the pair. The last branch's missed P1 lived in exactly that
  asymmetry: a guard field cleared on one path and not its twin.
- Supersession is a unit test, not only a device check: an announcement arriving
  mid-fetch discards the earlier result even when that fetch succeeds, and a failed
  fetch of a superseded blob surfaces nothing.
- An over-cap guest transfer writes no blob and announces nothing — asserted on the
  guest side, matching the phone side's 413, so the pair is covered symmetrically.
- An unsatisfiable `PasteRequested` answers with an empty transfer rather than
  falling silent. This test guards a hang, so it is not optional.
- On-device verification, which no suite substitutes for: image copy in both
  directions, and an image pending across a reconnect — expected outcome per §4,
  the phone does not hold the image afterward and a fresh guest copy delivers
  normally. Reconnect is where the last device-only bug hid.

## 9. Deliberately not in this design

- **File-transfer UX.** Own spec. The transport does not change to accommodate it;
  it will add a `DELETE`, a `filename` metadata field, and the server-named
  directory rule from §2.
- **Image transcoding.** See §7 — declining it is a security property, not a gap.
- **Resumable uploads and Range GETs.** The 64 MB ceiling makes a failed upload
  cheap to repeat.
- **A DELETE endpoint.** §3.
- **Clipboard replay on attach.** Consistent with the deliberate choice in the text
  clipboard design.
- **Primary-selection blobs.** `RequestDataTransfer(DataSource::Primary, ..)` is
  already an open follow-up on master and stays one.
