# Navette M1 — navetted Core Design

Status: Approved by M0 gate · Date: 2026-08-23

## Goal

Deliver the first useful Navette vertical slice: a stable versioned control
protocol, host app discovery, a persistent named-session registry, a process
supervisor, a loopback-safe WebSocket API, and a CLI that implements
`ls`, `run`, `attach`, `detach`, and `kill` against that API.

M1 uses stock `wprsd`/`wprsc` executables through process boundaries. It does
not link wprs crates and does not add media streaming; those remain M2 work.

## Safety boundary

`navetted` binds to `127.0.0.1:9417` by default. A non-loopback bind is rejected
unless the operator passes `--allow-remote`. Navette provides no application
authentication in M1, so tailnet or SSH transport remains the authentication
boundary. No public bind is implicit.

All runtime sockets live below `$XDG_RUNTIME_DIR/navette/`, created with
user-only directory permissions. Persistent state lives below
`$XDG_STATE_HOME/navette/` (or `~/.local/state/navette/`).

## Stable control protocol

The WebSocket endpoint is `/v1/ws` and negotiates subprotocol `navette.v1`.
Each text frame contains one JSON request or response. Binary frames are
reserved for M2 and rejected in M1.

Every request carries a client-generated numeric `request_id`. Requests and
responses use internally tagged snake-case enums so additions remain additive.

Requests:

- `list_apps`
- `list_sessions`
- `run { app_id, name? }`
- `kill { session }`
- `attach { session }`
- `detach { session }`
- `set_clipboard { text }`
- `get_clipboard`

Successful responses echo `request_id` and contain a tagged result. Failures
echo `request_id` when decodable and contain stable machine-readable codes:
`invalid_request`, `not_found`, `already_exists`, `invalid_name`,
`process_failed`, `unavailable`, and `internal`.

Shared types live in `navette-protocol`; no daemon-private type appears on the
wire. Timestamps are Unix milliseconds to keep Rust and future Kotlin clients
unambiguous.

## App index

Use `freedesktop-desktop-entry` to load entries in XDG priority order. Expose:

- desktop-entry ID
- localized name
- icon name/path as metadata only
- categories
- executable argv with desktop field codes removed
- terminal flag

Exclude `Hidden=true`, `NoDisplay=true`, non-application entries, entries with
no name/command, and entries whose `TryExec` cannot be resolved. Higher-priority
duplicate IDs win. Icon rendering is deferred to M4.

## Registry

M1 uses an atomic JSON state file rather than SQLite. The data volume is tiny,
the whole registry is rewritten on mutations, and this avoids a schema and C
dependency before query requirements exist.

Each session stores:

- validated name and source app ID
- app PID and `wprsd` PID
- per-session Wayland display and wprs socket path
- creation and last-attach timestamps
- client count
- lifecycle state (`starting`, `running`, `failed`, `stopped`)

Writes go to a sibling temporary file, are synced, and are atomically renamed.
On startup, recorded PIDs are probed. Dead sessions become `stopped`; Navette
does not pretend to restore application state after `wprsd` itself dies.

## Supervisor

The supervisor allocates deterministic resource names from the validated
session name:

```text
WAYLAND_DISPLAY=navette-<name>
$XDG_RUNTIME_DIR/navette/<name>/wprs.sock
```

Start sequence:

1. Reject duplicate or invalid session names.
2. Create the session runtime directory with mode `0700`.
3. Spawn `wprsd` with explicit display/socket arguments.
4. Wait for the Wayland and transport sockets with a bounded timeout.
5. Spawn the desktop entry's argv with `WAYLAND_DISPLAY` set.
6. Persist the running record.

If any step fails, terminate processes started by that attempt and record a
useful error. Process execution is behind a trait so unit tests use a fake
runner and never require wprs.

Kill sends SIGTERM to the application and daemon, waits briefly, then uses
SIGKILL only for survivors. Only PIDs created or reconciled from Navette's own
registry are targeted.

## Attach and detach

The API's attach response returns connection metadata, not a spawned local
client. The CLI uses it to execute `wprsc`:

- Local host: connect directly to the returned socket.
- Remote host: M1 accepts an operator-provided SSH target and creates local Unix
  socket forwarding before starting `wprsc`.
- Direct tailnet use applies to the WebSocket control API; wprs transport stays
  Unix-socket/SSH in M1 because stock wprs does not authenticate TCP sockets.

`detach` stops only the CLI-owned `wprsc`/forwarding process and reports the
detach to `navetted`; it never terminates the server-side app.

## Clipboard

M1 includes a text-only clipboard slot in the daemon because the roadmap
promotes text clipboard to Phase 0. The protocol supports get/set and the value
is memory-only in M1. Desktop clipboard integration is a later client concern;
the API contract lands now.

## Testing

- Protocol JSON fixtures and backward-compatible unknown-field acceptance.
- XDG parsing/filtering/precedence and Exec field-code handling.
- Registry atomic round trips and corrupt-file errors.
- Supervisor naming, duplicate handling, start rollback, and kill behavior with
  a fake process runner.
- WebSocket integration tests using an ephemeral loopback listener.
- CLI parser tests plus daemon-backed command tests where practical.
- Workspace `fmt`, `clippy -D warnings`, build, and tests stay green after each
  implementation commit.

## Deferred

- Linked wprs bridge and binary media channels (M2)
- H.264, input translation, and resize API (M2)
- Android and generated Kotlin types (M3)
- Icons/thumbnails and multi-host address book (M4)
- Application-layer auth, public listeners, TLS termination, and relays

