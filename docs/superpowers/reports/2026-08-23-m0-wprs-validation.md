# M0 wprs Adopt-vs-Fork Validation

Status: **PASS — adopt stock wprs**  
Date: 2026-08-23  
Host: ThinkPad (`tower`), Fedora 43, KDE Plasma/Wayland

## Decision

Proceed with stock wprs for Navette M1. The current upstream build supports the
two properties Navette needs immediately:

1. Multiple isolated `wprsd` instances under one user when each instance is
   given a distinct Wayland display and transport socket.
2. Client detach and reattach without terminating the applications owned by
   either daemon.

No fork or pre-M1 upstream contribution is justified by this run. Navette's
supervisor must treat `wayland_display` and `socket` as per-session resources
rather than relying on wprs defaults.

## Versions and build

- wprs upstream commit: `57139a03abf466c5f24b737835a61121bd25c8c0`
  (`bug fix: panic during attach when synching surfaces without buffers`)
- Upstream commit date: 2026-04-16
- Build: `cargo +stable build --release --bins`
- Result: successful release build with Fedora's installed
  `libxkbcommon-devel` and `wayland-devel` packages
- Firefox: Fedora package `154.0-1.fc43`
- Claude Desktop: `1.28929.0`, launched through its existing distrobox wrapper

The test used upstream binaries directly from a temporary checkout. It did not
install or vendor wprs into the Navette repository.

## Topology

Two independent daemon/client pairs ran simultaneously:

| Session | Remote display | Transport socket | Application |
|---|---|---|---|
| A | `wprs-navette-a` | `$XDG_RUNTIME_DIR/wprs-navette-a.sock` | Firefox, isolated profile |
| B | `wprs-navette-b` | `$XDG_RUNTIME_DIR/wprs-navette-b.sock` | Claude Desktop, isolated user-data directory |

XWayland was disabled for this gate. Both target applications used their native
Wayland paths, exercising the path Navette intends to support first.

## Results

### Concurrent instances

Both `wprsd` instances, both `wprsc` instances, and both applications remained
active concurrently. KDE reported reconstructed native windows with the
expected title prefixes:

```text
[M0 Firefox] Mozilla Firefox
[M0 Claude] Claude
```

A full-desktop screenshot confirmed that both reconstructed windows rendered.
The screenshot was retained only in `/tmp` because the desktop contained
unrelated private user content and is not suitable repository evidence.

### Detach and reattach

Before detach:

```text
Firefox PID:          3670784
Claude launcher PID:  3668106
```

Stopping both `wprsc` clients removed both prefixed local windows. Both daemon
services stayed active and both application PIDs remained unchanged. Starting
fresh `wprsc` clients recreated both windows, again with the expected titles,
while the application PIDs were still unchanged.

This satisfies the session-resumption and two-concurrent-instance portions of
M0.

### Socket permissions

The two transport sockets and two client control sockets were all owned by the
current user with mode `0600`:

```text
600 user:user $XDG_RUNTIME_DIR/wprs-navette-a.sock
600 user:user $XDG_RUNTIME_DIR/wprs-navette-b.sock
600 user:user $XDG_RUNTIME_DIR/wprsc-navette-a-ctrl.sock
600 user:user $XDG_RUNTIME_DIR/wprsc-navette-b-ctrl.sock
```

This matches the single-user trust boundary assumed by the PRP.

### Idle resource sample

Over a 10-second idle interval after reattach:

| Process | CPU used in interval | Memory |
|---|---:|---:|
| Firefox `wprsd` | 19 ms | 57.1 MiB |
| Claude `wprsd` | 0 ms | 38.2 MiB |
| Firefox `wprsc` | 18 ms | 124.8 MiB |
| Claude `wprsc` | 0 ms | 51.5 MiB |

The idle cost is acceptable for the one-daemon-per-session architecture. This
sample is a smoke measurement, not a performance benchmark.

### Input path

KDE successfully activated the reconstructed Firefox window, and the wprs logs
showed keyboard focus/keymap synchronization for reconstructed surfaces.
Synthetic key injection could not be completed because this execution
environment cannot open `/dev/uinput`, even through `sudo`; no host security
settings were weakened to bypass that restriction. Interactive typing remains
a short manual confirmation, not an adoption blocker.

## Observed limitations

- wprs/Smithay logged `Output is used with not preferred mode set` for the
  ThinkPad's high-DPI output. Rendering continued.
- Claude Desktop was run with `--disable-gpu`, and Firefox used the shm-capable
  Wayland path. The gate therefore validates the PRP's software-rendered target
  workload, not dmabuf or GPU-heavy applications.
- This run used local Unix sockets on the host. LAN/tailnet latency and SSH
  forwarding measurements need a second client endpoint and are deferred.
- Touch, dmabuf, and webauthn remain upstream limitations and are unchanged by
  this decision.

## M1 implications

- Allocate a unique Wayland display and wprs socket for every Navette session.
- Store both paths in the session registry and never assume upstream defaults.
- Keep wprs execution behind a process-runner boundary so M1 tests can use a
  fake runner without requiring wprs to be installed.
- Do not add a `navette-bridge` dependency on wprs yet; that remains M2 work.
- Treat tailnet/SSH transport benchmarking as an integration check when a
  second endpoint is available, not as a blocker for the local M1 supervisor
  and registry.
