//! End-to-end cover for the media protocol: a real `navetted` router, a real
//! WebSocket, and a real `MediaClient` decoding what the hub replays.

use std::collections::BTreeSet;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use navette_protocol::media::{
    MediaFlags, MediaHeader, MediaInput, MediaKind, MediaPacket, StreamConfig as MediaStreamConfig,
};
use navette_protocol::{App, Session, SessionStatus};
use navette_viewer::client::INPUT_QUEUE_CAPACITY;
use navette_viewer::{
    ClientError, DecodedFrame, Decoder, DecoderConfig, DecoderError, DecoderMetrics, FakeDecoder,
    MediaClient, StreamEvent, StreamFrame, media_url,
};
use navetted::api::{ApiState, router};
use navetted::app_index::AppIndex;
use navetted::media::MediaCommand;
use navetted::registry::Registry;
use navetted::supervisor::{ProcessRunner, ProcessSpec, Supervisor};
use tempfile::TempDir;

const CODEC_CONFIG: [u8; 12] = [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 1];

#[derive(Debug, Default)]
struct NoopRunner {
    alive: Mutex<BTreeSet<u32>>,
}

impl ProcessRunner for NoopRunner {
    fn spawn(&self, _spec: ProcessSpec) -> io::Result<u32> {
        Err(io::Error::other("spawn is not used by this test"))
    }

    fn is_alive(&self, pid: u32) -> bool {
        self.alive.lock().is_ok_and(|alive| alive.contains(&pid))
    }

    fn terminate(&self, _pid: u32, _force: bool) -> io::Result<()> {
        Ok(())
    }
}

fn test_state(temp: &TempDir) -> ApiState<NoopRunner> {
    let apps = AppIndex::from_apps([App {
        id: "firefox".into(),
        name: "Firefox".into(),
        icon: None,
        categories: vec!["Network".into()],
        exec: vec!["firefox".into()],
        terminal: false,
    }]);
    let registry = Registry::open(temp.path().join("registry.json")).unwrap();
    let supervisor = Supervisor::new(
        Arc::new(NoopRunner::default()),
        Arc::new(Mutex::new(registry)),
        temp.path().join("runtime"),
        "wprsd",
    )
    .with_timeouts(
        Duration::from_millis(5),
        Duration::from_millis(5),
        Duration::from_millis(1),
    );
    ApiState::new(Arc::new(apps), Arc::new(supervisor))
}

fn add_running_session(state: &ApiState<NoopRunner>, name: &str) {
    state
        .supervisor
        .registry()
        .lock()
        .unwrap()
        .insert(Session {
            name: name.into(),
            app_id: "firefox".into(),
            app_pid: 10,
            daemon_pid: 11,
            wayland_display: format!("navette-{name}"),
            socket_path: format!("/tmp/{name}.sock"),
            created_at_ms: 1,
            last_attached_at_ms: None,
            client_count: 0,
            status: SessionStatus::Running,
        })
        .unwrap();
}

fn header(kind: MediaKind, stream_id: u64, sequence: u64, keyframe: bool) -> MediaHeader {
    MediaHeader {
        kind,
        flags: MediaFlags::new(keyframe, false),
        stream_id,
        sequence,
        timestamp_us: sequence * 1000,
        payload_len: 0,
        width: 64,
        height: 32,
    }
}

fn stream_config_packet(stream_id: u64, sequence: u64) -> MediaPacket {
    let payload = MediaStreamConfig {
        client_id: 11,
        surface_id: 12,
        codec_config: CODEC_CONFIG.to_vec(),
    }
    .encode()
    .unwrap();
    MediaPacket::new(
        header(MediaKind::StreamConfig, stream_id, sequence, false),
        payload,
    )
    .unwrap()
}

fn video_packet(stream_id: u64, sequence: u64) -> MediaPacket {
    MediaPacket::new(
        header(MediaKind::Video, stream_id, sequence, true),
        vec![0, 0, 0, 1, 0x65, sequence as u8],
    )
    .unwrap()
}

/// Waits for the next decoded picture, stepping over the per-packet
/// accounting the HUD consumes.
async fn next_frame(client: &mut MediaClient) -> StreamFrame {
    loop {
        match client.next_event().await {
            Some(StreamEvent::Packet(_)) => continue,
            Some(StreamEvent::Frame(frame)) => return frame,
            other => panic!("expected a decoded frame, got {other:?}"),
        }
    }
}

/// Signals on drop, so a test can observe the decode thread actually
/// finishing rather than assuming it did.
struct DropSignallingDecoder {
    config: DecoderConfig,
    /// Dropped with the decoder; its receiver disconnects when the decode
    /// thread lets the router go.
    _alive: std::sync::mpsc::Sender<()>,
}

impl Decoder for DropSignallingDecoder {
    fn config(&self) -> &DecoderConfig {
        &self.config
    }
    fn decode(&mut self, _access_unit: &[u8]) -> Result<Vec<DecodedFrame>, DecoderError> {
        Ok(Vec::new())
    }
    fn drain(&mut self) -> Vec<DecodedFrame> {
        Vec::new()
    }
    fn reconfigure(&mut self, config: DecoderConfig) -> Result<(), DecoderError> {
        self.config = config;
        Ok(())
    }
    fn metrics(&self) -> DecoderMetrics {
        DecoderMetrics::default()
    }
}

/// Dropping the client must stop the decode thread.
///
/// The thread cannot be aborted the way the old inline task could, so its
/// shutdown is entirely by channel closure: dropping the client aborts the
/// connection task, which drops the packet sender, which ends the thread's
/// `blocking_recv`. If that reasoning were wrong every client would leak a
/// thread parked in a blocking FFmpeg read, and nothing else in the suite
/// would notice -- so this observes the decoder being dropped rather than
/// trusting the argument.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_client_stops_the_decode_thread() {
    let temp = TempDir::new().unwrap();
    let state = test_state(&temp);
    add_running_session(&state, "work");
    let _input = state.media.register_session("work");
    state
        .media
        .publish("work", stream_config_packet(1, 1))
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });

    // Held by the decoder; disconnects once the decoder is dropped, which
    // only happens when the decode thread drops the router and exits.
    let (alive, exited) = std::sync::mpsc::channel();
    let alive = std::sync::Mutex::new(Some(alive));
    let (built, ready) = std::sync::mpsc::channel();

    let url = media_url(&format!("ws://{address}"), "work");
    let client = MediaClient::connect(
        &url,
        Box::new(move |config: &DecoderConfig| {
            // Announce construction, so the test waits for the decoder to
            // exist instead of sleeping and hoping.
            let _ = built.send(());
            Ok(Box::new(DropSignallingDecoder {
                config: config.clone(),
                _alive: alive.lock().unwrap().take().expect("one decoder"),
            }) as Box<dyn Decoder>)
        }),
    )
    .await
    .unwrap();

    // Wait for the decoder to exist rather than sleeping: if it were never
    // built, `exited` would report disconnected immediately and this would
    // "pass" without ever exercising the thread's shutdown.
    ready
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the decoder must be built before the client is dropped");
    assert!(
        exited.try_recv().is_err(),
        "decoder should still be alive while the client is"
    );

    drop(client);

    // `Disconnected` means the decoder was dropped: the thread left its loop.
    match exited.recv_timeout(std::time::Duration::from_secs(5)) {
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
        other => panic!("decode thread did not exit after the client dropped: {other:?}"),
    }

    server.abort();
}

/// A decoder that parks in `decode` until released, so a test can hold the
/// decode pipeline mid-packet and observe what the rest of the client does
/// while it is stuck.
struct BlockingDecoder {
    config: DecoderConfig,
    /// Signalled on entry to `decode`, so a test can wait for the pipeline to
    /// actually be parked rather than sleeping and hoping.
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

impl Decoder for BlockingDecoder {
    fn config(&self) -> &DecoderConfig {
        &self.config
    }

    fn decode(&mut self, _access_unit: &[u8]) -> Result<Vec<DecodedFrame>, DecoderError> {
        // Stands in for the real cost: a stream reconfigure spawning a fresh
        // FFmpeg process, measured at ~600ms.
        let _ = self.entered.send(());
        let _ = self.release.recv();
        Ok(Vec::new())
    }

    fn drain(&mut self) -> Vec<DecodedFrame> {
        Vec::new()
    }

    fn reconfigure(&mut self, config: DecoderConfig) -> Result<(), DecoderError> {
        self.config = config;
        Ok(())
    }

    fn metrics(&self) -> DecoderMetrics {
        DecoderMetrics::default()
    }
}

/// Input reaches the session while the decode pipeline is blocked.
///
/// This is the regression net for the defect that shipped in #8: decoding ran
/// inline on the connection task, so while a packet was being decoded the
/// `select!` had already committed to that branch and stopped polling
/// `inputs.recv()`. Every keystroke queued during a ~600ms stream reconfigure
/// sat undelivered until it finished. Decoding now owns a thread, so this
/// asserts the property directly: hold the decoder mid-packet, send input,
/// and require it on the wire *before* the decoder is released.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_is_delivered_while_the_decoder_is_blocked() {
    let temp = TempDir::new().unwrap();
    let state = test_state(&temp);
    add_running_session(&state, "work");
    let mut input = state.media.register_session("work");
    state
        .media
        .publish("work", stream_config_packet(1, 1))
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hub = state.media.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });

    let (release, blocked) = std::sync::mpsc::channel();
    let blocked = std::sync::Mutex::new(Some(blocked));
    let (entered, decoding) = std::sync::mpsc::channel();
    let url = media_url(&format!("ws://{address}"), "work");
    let client = MediaClient::connect(
        &url,
        Box::new(move |config: &DecoderConfig| {
            Ok(Box::new(BlockingDecoder {
                config: config.clone(),
                entered: entered.clone(),
                release: blocked.lock().unwrap().take().expect("one decoder"),
            }) as Box<dyn Decoder>)
        }),
    )
    .await
    .unwrap();

    // The connect-time keyframe request, sent before any of this.
    assert_eq!(
        input.recv().await,
        Some(MediaCommand::Input {
            attachment_id: 1,
            input: navette_protocol::media::MediaInput::RequestKeyframe,
        })
    );

    // Wedge the decode pipeline inside this packet, and *wait for it to
    // actually be wedged*. Sleeping here instead would let the test pass
    // vacuously on a slow machine: if the packet had not yet reached the
    // decode thread, nothing would be blocking and the input would sail
    // through for reasons unrelated to what this test checks.
    hub.publish("work", video_packet(1, 2)).unwrap();
    decoding
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the decoder must be reached and parked before input is sent");

    // The decoder is now parked. Input queued here must not wait for it.
    client
        .send_input(navette_protocol::media::MediaInput::ViewportResize {
            width: 800,
            height: 600,
        })
        .unwrap();

    let delivered = tokio::time::timeout(std::time::Duration::from_secs(2), input.recv())
        .await
        .expect("input must reach the session while the decoder is blocked");
    assert_eq!(
        delivered,
        Some(MediaCommand::Input {
            attachment_id: 1,
            input: navette_protocol::media::MediaInput::ViewportResize {
                width: 800,
                height: 600,
            },
        })
    );

    // Only now let the decoder go, proving it really was parked throughout.
    drop(release);
    server.abort();
}

/// The same flow on a multi-threaded runtime, which is the flavour the binary
/// actually uses (`#[tokio::main]` defaults to it).
///
/// It used to guard a runtime-flavour branch in the decode path; that branch
/// is gone now that decoding owns a thread instead of borrowing a runtime
/// worker. It is kept because it is still the only coverage of this client
/// under the flavour production runs on, and the decode split it now exercises
/// -- a real thread handing frames back over a channel -- has more moving
/// parts across threads than the version it replaced, not fewer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_decodes_on_a_multi_thread_runtime() {
    let temp = TempDir::new().unwrap();
    let state = test_state(&temp);
    add_running_session(&state, "work");
    let _input = state.media.register_session("work");
    state
        .media
        .publish("work", stream_config_packet(1, 1))
        .unwrap();
    state.media.publish("work", video_packet(1, 2)).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });

    let url = media_url(&format!("ws://{address}"), "work");
    let mut client = MediaClient::connect(
        &url,
        Box::new(|config: &DecoderConfig| {
            Ok(Box::new(FakeDecoder::new(config.clone())?) as Box<dyn Decoder>)
        }),
    )
    .await
    .unwrap();

    let frame = next_frame(&mut client).await;
    assert_eq!(frame.stream_id, 1);
    assert_eq!((frame.frame.width, frame.frame.height), (64, 32));

    server.abort();
}

#[tokio::test]
async fn client_decodes_the_bootstrap_replayed_on_attach() {
    let temp = TempDir::new().unwrap();
    let state = test_state(&temp);
    add_running_session(&state, "work");
    let mut input = state.media.register_session("work");
    state
        .media
        .publish("work", stream_config_packet(1, 1))
        .unwrap();
    state.media.publish("work", video_packet(1, 2)).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hub = state.media.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });

    let url = media_url(&format!("ws://{address}"), "work");
    let mut client = MediaClient::connect(
        &url,
        Box::new(|config: &DecoderConfig| {
            Ok(Box::new(FakeDecoder::new(config.clone())?) as Box<dyn Decoder>)
        }),
    )
    .await
    .unwrap();

    // The keyframe request the client sends on connect reaches the bridge.
    assert_eq!(
        input.recv().await,
        Some(MediaCommand::Input {
            attachment_id: 1,
            input: navette_protocol::media::MediaInput::RequestKeyframe,
        })
    );

    // Each packet is accounted for before whatever it decoded into, so the
    // HUD sees a packet's wire cost and sequence alongside its own frames.
    for (kind, sequence) in [(MediaKind::StreamConfig, 1), (MediaKind::Video, 2)] {
        match client.next_event().await {
            Some(StreamEvent::Packet(packet)) => {
                assert_eq!((packet.kind, packet.sequence), (kind, sequence));
                assert_eq!(packet.stream_id, 1);
                assert!(packet.wire_bytes > 0);
            }
            other => panic!("expected accounting for the {kind:?} packet, got {other:?}"),
        }
    }

    let frame = next_frame(&mut client).await;
    assert_eq!(frame.stream_id, 1);
    assert_eq!(frame.client_id, 11);
    assert_eq!(frame.surface_id, 12);
    assert_eq!((frame.frame.width, frame.frame.height), (64, 32));
    assert_eq!(frame.frame.pixels.len(), 64 * 32 * 4);

    // Live traffic published after the attach keeps flowing to the same
    // decoder, and closing the toplevel tears that stream down.
    hub.publish("work", video_packet(1, 3)).unwrap();
    assert_eq!(next_frame(&mut client).await.frame.pixels[0], 1);

    hub.publish(
        "work",
        MediaPacket::new(header(MediaKind::StreamEnd, 1, 4, false), Vec::new()).unwrap(),
    )
    .unwrap();
    loop {
        match client.next_event().await {
            Some(StreamEvent::Packet(_)) => continue,
            other => {
                assert_eq!(other, Some(StreamEvent::Ended { stream_id: 1 }));
                break;
            }
        }
    }

    server.abort();
}

#[tokio::test]
async fn window_input_reaches_the_bridge_over_the_same_connection() {
    let temp = TempDir::new().unwrap();
    let state = test_state(&temp);
    add_running_session(&state, "work");
    let mut commands = state.media.register_session("work");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });

    let url = media_url(&format!("ws://{address}"), "work");
    let client = MediaClient::connect(
        &url,
        Box::new(|config: &DecoderConfig| {
            Ok(Box::new(FakeDecoder::new(config.clone())?) as Box<dyn Decoder>)
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        commands.recv().await,
        Some(MediaCommand::Input {
            attachment_id: 1,
            input: MediaInput::RequestKeyframe,
        })
    );

    // Out-of-range input is refused locally and never queued, so it cannot
    // arrive ahead of — or instead of — the valid message that follows it.
    let refused = client.send_input(MediaInput::ViewportResize {
        width: 16,
        height: 16,
    });
    assert!(
        matches!(refused, Err(ClientError::InvalidInput(_))),
        "expected a local rejection, got {refused:?}"
    );

    let pointer = MediaInput::PointerMotion {
        client_id: 11,
        surface_id: 12,
        x: 4.5,
        y: 6.5,
    };
    let resize = MediaInput::ViewportResize {
        width: 1280,
        height: 720,
    };
    client.send_input(pointer.clone()).unwrap();
    client.send_input(resize.clone()).unwrap();

    for input in [pointer.clone(), resize] {
        assert_eq!(
            commands.recv().await,
            Some(MediaCommand::Input {
                attachment_id: 1,
                input,
            })
        );
    }

    // Sending must never wait on the connection: a window loop drives both
    // this and `next_event`, so a send that blocked would stop the loop
    // draining events, which would block the connection task, which is the
    // only thing that drains input — a deadlock neither side can break.
    //
    // `send_input` returns without awaiting, so on this single-threaded test
    // runtime the connection task cannot be scheduled part-way through the
    // loop below and the queue is guaranteed to saturate. Every call still
    // returns; the overflow is refused rather than waited on.
    let overflow = 8;
    let mut refused = 0;
    for _ in 0..(INPUT_QUEUE_CAPACITY + overflow) {
        match client.send_input(pointer.clone()) {
            Ok(()) => {}
            Err(ClientError::InputBackpressure) => refused += 1,
            Err(error) => panic!("unexpected send failure: {error}"),
        }
    }
    assert_eq!(
        refused, overflow,
        "a saturated queue must refuse the excess rather than wait for room"
    );

    server.abort();
}

#[tokio::test]
async fn connect_fails_when_the_session_is_not_running() {
    let temp = TempDir::new().unwrap();
    let state = test_state(&temp);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });

    let url = media_url(&format!("ws://{address}"), "missing");
    let outcome = MediaClient::connect(
        &url,
        Box::new(|config: &DecoderConfig| {
            Ok(Box::new(FakeDecoder::new(config.clone())?) as Box<dyn Decoder>)
        }),
    )
    .await;
    match outcome {
        Ok(_) => panic!("connecting to an unknown session should fail"),
        Err(error) => assert!(
            error.to_string().contains("404"),
            "unexpected error: {error}"
        ),
    }

    server.abort();
}
