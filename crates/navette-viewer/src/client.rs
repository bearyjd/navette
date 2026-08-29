use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;

use navette_protocol::media::{
    InputValidationError, MEDIA_HEADER_LEN, MEDIA_WEBSOCKET_SUBPROTOCOL, MediaInput, MediaPacket,
};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::router::{DecoderFactory, StreamEvent, StreamPacket, StreamRouter};

/// Every packet now yields an accounting event as well as its frames, so this
/// carries roughly twice the traffic it used to; too shallow a queue would
/// leave the connection task parked mid-packet and slow to service input.
const EVENT_QUEUE_CAPACITY: usize = 32;

/// Input is queued rather than written inline so a window loop never blocks on
/// the socket. The bound is deep enough for a burst of key and pointer events
/// while a frame is being decoded, and shallow enough that a queue this long
/// means something is genuinely wrong.
pub const INPUT_QUEUE_CAPACITY: usize = 64;

/// Packets that may be waiting on the decode thread before the socket is
/// slowed down.
///
/// This is the headroom that keeps input flowing while the decoder is busy:
/// once it is exhausted the connection task parks handing over a packet and
/// stops draining input again, which is the stall this split exists to
/// remove. Sized against the worst real case measured -- a stream
/// reconfigure spawning a fresh FFmpeg process, ~600ms, against a stream
/// running at tens of packets a second -- with room to spare, since the cost
/// of being generous is only buffered packets and the cost of being tight is
/// the bug coming back.
const PACKET_QUEUE_CAPACITY: usize = 128;

/// Payload bytes allowed to sit queued for the decode thread, in KiB.
///
/// A count alone is not a memory bound: the protocol permits payloads up to
/// `MAX_MEDIA_PAYLOAD` (16 MiB), so 128 queued packets is 2 GiB in the worst
/// case a peer can construct, where before this queue existed only one packet
/// was ever in flight. Real traffic is nothing like that -- measured against a
/// live session, frames run a median of 1.6 KiB and a maximum of 18 KiB -- so
/// this budget still buys hundreds of ordinary packets of headroom, far more
/// than the ~32 needed to cover a reconfigure, while capping what a
/// misbehaving or hostile server can make this client allocate.
const PACKET_QUEUE_KIB: u32 = 8 * 1024;
const SUBPROTOCOL_HEADER: &str = "Sec-WebSocket-Protocol";

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Sink = SplitSink<Socket, Message>;

/// Client for one session's media endpoint.
///
/// The client owns the socket and a [`StreamRouter`]; decoded frames and
/// stream lifecycle events are handed to the caller through a bounded channel,
/// so a consumer that falls behind exerts backpressure on the socket instead
/// of growing an unbounded backlog. Decoding runs on a thread of its own
/// rather than on the connection task: it blocks for as long as FFmpeg takes,
/// which on a stream reconfigure is a process spawn, and doing that on the
/// connection task stalled input delivery and every timer in the process.
pub struct MediaClient {
    events: mpsc::Receiver<StreamEvent>,
    inputs: mpsc::Sender<MediaInput>,
    connection: JoinHandle<()>,
}

impl MediaClient {
    /// Connects to `url` (`ws://host/v1/sessions/{session}/media`), negotiates
    /// the media subprotocol and asks the bridge for an immediate keyframe.
    pub async fn connect(url: &str, factory: DecoderFactory) -> Result<Self, ClientError> {
        let mut request = url
            .into_client_request()
            .map_err(|error| ClientError::Connect(Box::new(error)))?;
        let protocol = MEDIA_WEBSOCKET_SUBPROTOCOL
            .parse()
            .map_err(|_| ClientError::Subprotocol)?;
        request.headers_mut().insert(SUBPROTOCOL_HEADER, protocol);
        let (socket, response) = connect_async(request)
            .await
            .map_err(|error| ClientError::Connect(Box::new(error)))?;
        if response
            .headers()
            .get(SUBPROTOCOL_HEADER)
            .is_none_or(|value| value.as_bytes() != MEDIA_WEBSOCKET_SUBPROTOCOL.as_bytes())
        {
            return Err(ClientError::Subprotocol);
        }

        let (mut sink, stream) = socket.split();
        send_input(&mut sink, MediaInput::RequestKeyframe).await?;
        let (sender, events) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        let (inputs, input_queue) = mpsc::channel(INPUT_QUEUE_CAPACITY);
        let (packets, packet_queue) = mpsc::channel(PACKET_QUEUE_CAPACITY);
        let budget = Arc::new(Semaphore::new(PACKET_QUEUE_KIB as usize));
        let router = StreamRouter::new(factory);
        // A dedicated one-slot channel for keyframe requests rather than a
        // clone of `inputs`. Capacity one makes `Full` mean "a request is
        // already pending", which is exactly right for an idempotent
        // `RequestKeyframe`, and the connection task drains it from its own
        // `select!` arm so the decode thread never has to block to be heard.
        let (keyframes, keyframe_queue) = mpsc::channel(1);
        std::thread::Builder::new()
            .name("navette-decode".to_owned())
            .spawn(move || decode(router, packet_queue, sender, keyframes))
            .map_err(ClientError::DecodeThread)?;
        let connection = tokio::spawn(run(
            stream,
            sink,
            packets,
            input_queue,
            keyframe_queue,
            budget,
        ));
        Ok(Self {
            events,
            inputs,
            connection,
        })
    }

    /// Yields the next decoded frame or stream lifecycle event, or `None` once
    /// the connection has closed.
    pub async fn next_event(&mut self) -> Option<StreamEvent> {
        self.events.recv().await
    }

    /// Queues one input event for the attached session.
    ///
    /// The payload is validated against the protocol's own bounds first: the
    /// server rejects out-of-range input anyway, but a client that never sends
    /// it keeps a local bug from looking like a protocol violation on the
    /// wire.
    ///
    /// This never waits, and that is load-bearing rather than a convenience.
    /// A window loop drives both halves of this client: it drains
    /// [`next_event`](Self::next_event) and it feeds this. If sending could
    /// block on a full queue, the loop would stop draining events; the
    /// connection task would then fill the event queue and block in turn; and
    /// the only task that drains input would be the one now waiting on the
    /// consumer that is waiting on it. Refusing input under saturation costs
    /// at worst a dropped event — the bridge releases a session's held keys
    /// and buttons when it detaches — where waiting would cost the session.
    pub fn send_input(&self, input: MediaInput) -> Result<(), ClientError> {
        input.validate().map_err(ClientError::InvalidInput)?;
        self.inputs.try_send(input).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => ClientError::InputBackpressure,
            mpsc::error::TrySendError::Closed(_) => ClientError::Disconnected,
        })
    }
}

impl Drop for MediaClient {
    /// Aborting the connection task drops its packet sender, which closes the
    /// decode thread's channel and lets that thread fall out of its loop on
    /// its own. It is deliberately not joined: the thread may be inside a
    /// blocking FFmpeg read, and blocking `drop` on it could stall the caller
    /// for as long as a reconfigure takes.
    fn drop(&mut self) {
        self.connection.abort();
    }
}

/// A packet plus the slice of the queue's byte budget it occupies. Dropping
/// the permit returns that budget, so it is deliberately carried all the way
/// to the end of the decode thread's work rather than released on receipt.
type QueuedPacket = (MediaPacket, OwnedSemaphorePermit);

/// Reads the socket and writes input to it, and does nothing that blocks.
///
/// Decoding used to happen inline here, which meant that while a packet was
/// being decoded the `select!` had already committed to this branch and was
/// no longer polling `inputs.recv()`. A stream reconfigure spawns a fresh
/// FFmpeg process and waits ~600ms for it to prime, so every keystroke and
/// pointer movement queued during that window sat undelivered until it
/// finished. Decoding now happens on its own thread; this task only hands
/// packets over, so input keeps flowing while the decoder works.
async fn run(
    mut stream: SplitStream<Socket>,
    mut sink: Sink,
    packets: mpsc::Sender<QueuedPacket>,
    mut inputs: mpsc::Receiver<MediaInput>,
    mut keyframes: mpsc::Receiver<()>,
    budget: Arc<Semaphore>,
) {
    loop {
        tokio::select! {
            message = stream.next() => {
                let Some(message) = message else { break };
                if !receive(message, &packets, &budget).await {
                    break;
                }
            }
            // Recovery from a decode failure depends on this reaching the
            // bridge: the router discards the decoder for a failed stream, so
            // nothing rebuilds it until a fresh `stream_config` arrives, and
            // nothing produces one until this request does.
            Some(()) = keyframes.recv() => {
                if send_input(&mut sink, MediaInput::RequestKeyframe).await.is_err() {
                    break;
                }
            }
            input = inputs.recv() => {
                // `None` only happens once the owning `MediaClient` is gone,
                // which also aborts this task.
                let Some(input) = input else { break };
                if send_input(&mut sink, input).await.is_err() {
                    break;
                }
            }
        }
    }
    tracing::debug!("media connection closed");
}

/// Owns the decode pipeline, on a thread of its own.
///
/// A real thread rather than a task because this work is *inherently*
/// blocking -- it writes to FFmpeg's stdin, reads frames back, and on a
/// reconfigure spawns a new process and waits for it to prime. Blocking
/// inside an async task is what previously stalled every timer in the
/// process: exactly one runtime worker at a time holds tokio's I/O+time
/// driver, and when that worker's task blocks nothing re-enters the driver,
/// so `tokio::time` stops firing entirely. Giving the pipeline its own thread
/// removes that failure mode rather than mitigating it.
///
/// Exits when `packets` closes, which happens once the connection task is
/// gone and its sender drops.
fn decode(
    mut router: StreamRouter,
    mut packets: mpsc::Receiver<QueuedPacket>,
    events: mpsc::Sender<StreamEvent>,
    keyframes: mpsc::Sender<()>,
) {
    // `permit` is held for the whole iteration and released with it, so the
    // budget reflects packets still owned by this thread, not merely queued.
    while let Some((packet, _permit)) = packets.blocking_recv() {
        let decoded = router.handle(&packet);
        // Accounting first, and from this same thread, so a packet's wire
        // cost is always reported before whatever it decoded into.
        // `observe` reads the router's metrics *after* `handle` updated them.
        let observation = observe(&packet, &router);
        if events
            .blocking_send(StreamEvent::Packet(observation))
            .is_err()
        {
            break;
        }
        for event in decoded {
            if let StreamEvent::DecodeFailed { stream_id } = event {
                tracing::info!(stream_id, "requesting a keyframe after a decode failure");
                // Never a blocking send: the connection task drains this and
                // may itself be parked handing this thread a packet, which
                // would close into a deadlock. `Full` here is not a loss --
                // the channel holds one slot and `RequestKeyframe` carries no
                // stream identity, so a request is already queued and will be
                // sent. This must not simply be dropped: the router discards
                // a failed stream's decoder (`router.rs`), so `DecodeFailed`
                // fires exactly once per decoder and nothing else asks again.
                // Losing it leaves that surface black until the user resizes
                // or reattaches.
                if let Err(mpsc::error::TrySendError::Closed(())) = keyframes.try_send(()) {
                    tracing::debug!(stream_id, "connection gone; cannot request a keyframe");
                }
            }
            if events.blocking_send(event).is_err() {
                return;
            }
        }
    }
    tracing::debug!("decode pipeline stopped");
}

/// Hands one received WebSocket message to the decode thread. Returns `false`
/// when the connection must be torn down.
async fn receive(
    message: Result<Message, tokio_tungstenite::tungstenite::Error>,
    packets: &mpsc::Sender<QueuedPacket>,
    budget: &Arc<Semaphore>,
) -> bool {
    let message = match message {
        Ok(message) => message,
        Err(error) => {
            tracing::debug!(%error, "media WebSocket receive failed");
            return false;
        }
    };
    match message {
        // Awaiting here is deliberate backpressure: a decoder that cannot
        // keep up slows the socket rather than growing an unbounded backlog,
        // which is the same contract this client has always had. It does mean
        // input delivery stalls again once `PACKET_QUEUE_CAPACITY` is
        // exhausted, so that capacity is sized against a real reconfigure.
        Message::Binary(bytes) => match MediaPacket::decode(&bytes) {
            Ok(packet) => {
                // Charged in KiB, always at least one, so a flood of empty
                // packets is still bounded by the queue's own capacity.
                let kib = u32::try_from(packet.payload.len().div_ceil(1024))
                    .unwrap_or(u32::MAX)
                    .clamp(1, PACKET_QUEUE_KIB);
                let Ok(permit) = Arc::clone(budget).acquire_many_owned(kib).await else {
                    return false;
                };
                packets.send((packet, permit)).await.is_ok()
            }
            Err(error) => {
                tracing::warn!(%error, "discarding malformed media packet");
                true
            }
        },
        // The server reports protocol problems as JSON text; they are
        // informational and must not take the connection down.
        Message::Text(text) => {
            tracing::warn!(%text, "media server reported an error");
            true
        }
        Message::Close(_) => false,
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => true,
    }
}

fn observe(packet: &MediaPacket, router: &StreamRouter) -> StreamPacket {
    StreamPacket {
        stream_id: packet.header.stream_id,
        kind: packet.header.kind,
        sequence: packet.header.sequence,
        wire_bytes: MEDIA_HEADER_LEN.saturating_add(packet.payload.len()),
        discontinuity: packet.header.flags.discontinuity(),
        decoder: router.metrics(packet.header.stream_id),
    }
}

#[cfg(test)]
mod tests {
    use navette_protocol::media::{
        MediaFlags, MediaHeader, MediaKind, StreamConfig as MediaStreamConfig,
    };

    use super::*;
    use crate::decoder::{DecodedFrame, DecoderConfig, DecoderError, DecoderMetrics};
    use crate::router::StreamEvent;

    /// Fails every access unit, so the router raises `DecodeFailed` and the
    /// decode loop tries to ask the bridge for a keyframe.
    struct AlwaysFails(DecoderConfig);

    impl crate::decoder::Decoder for AlwaysFails {
        fn config(&self) -> &DecoderConfig {
            &self.0
        }
        fn decode(&mut self, _access_unit: &[u8]) -> Result<Vec<DecodedFrame>, DecoderError> {
            Err(DecoderError::EmptyAccessUnit)
        }
        fn drain(&mut self) -> Vec<DecodedFrame> {
            Vec::new()
        }
        fn reconfigure(&mut self, config: DecoderConfig) -> Result<(), DecoderError> {
            self.0 = config;
            Ok(())
        }
        fn metrics(&self) -> DecoderMetrics {
            DecoderMetrics::default()
        }
    }

    fn header(kind: MediaKind, sequence: u64) -> MediaHeader {
        MediaHeader {
            kind,
            flags: MediaFlags::new(true, false),
            stream_id: 1,
            sequence,
            timestamp_us: sequence * 1000,
            payload_len: 0,
            width: 64,
            height: 32,
        }
    }

    /// A packet larger than the whole byte budget must still get through.
    ///
    /// Payloads are charged against a fixed budget, and the protocol allows a
    /// single payload (up to `MAX_MEDIA_PAYLOAD`, 16 MiB) to exceed that
    /// budget outright. Without the clamp, acquiring that many permits could
    /// never succeed and the connection task would park forever on a packet
    /// it is holding the only copy of -- a hang, not a slow path.
    #[tokio::test]
    async fn a_packet_larger_than_the_budget_is_still_queued() {
        let (packets, mut packet_queue) = mpsc::channel(4);
        let budget = Arc::new(Semaphore::new(PACKET_QUEUE_KIB as usize));

        // One KiB past the entire budget.
        let oversized = vec![0u8; (PACKET_QUEUE_KIB as usize + 1) * 1024];
        let packet = MediaPacket::new(header(MediaKind::Video, 1), oversized).unwrap();
        let encoded = packet.encode().unwrap();

        let queued = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            receive(Ok(Message::Binary(encoded.into())), &packets, &budget),
        )
        .await
        .expect("receive must not park forever on a packet bigger than the budget");
        assert!(queued, "an oversized packet should still be handed on");
        assert!(packet_queue.try_recv().is_ok(), "packet should be queued");
    }

    /// A keyframe request survives a busy connection instead of being lost.
    ///
    /// This asserts the opposite of what it did when first written. The
    /// original justified dropping the request as self-healing, on the theory
    /// that the next packet would raise `DecodeFailed` again. It does not:
    /// the router *removes* a failed stream's decoder, so every later video
    /// packet for it is discarded as unconfigured and `DecodeFailed` fires
    /// exactly once per decoder lifetime (`router.rs` has a test asserting
    /// precisely that). Nothing rebuilds the decoder until a fresh
    /// `stream_config` arrives, and nothing produces one until this request
    /// reaches the bridge -- so losing it leaves that surface black until the
    /// user resizes or reattaches.
    ///
    /// The request therefore goes to a dedicated one-slot channel rather than
    /// the shared input queue: it can never be crowded out by input, and
    /// `Full` means "already pending" rather than "dropped".
    #[test]
    fn a_decode_failure_always_produces_a_keyframe_request() {
        let (keyframes, mut keyframe_queue) = mpsc::channel(1);
        let (packets, packet_queue) = mpsc::channel(4);
        let (events, mut event_rx) = mpsc::channel(16);

        let router = StreamRouter::new(Box::new(|config: &DecoderConfig| {
            Ok(Box::new(AlwaysFails(config.clone())) as Box<dyn crate::decoder::Decoder>)
        }));

        let config = MediaStreamConfig {
            client_id: 11,
            surface_id: 12,
            codec_config: vec![0, 0, 0, 1, 0x67, 0x42],
        }
        .encode()
        .unwrap();
        let budget = Arc::new(Semaphore::new(8));
        let permit = |n| Arc::clone(&budget).try_acquire_many_owned(n).unwrap();
        packets
            .try_send((
                MediaPacket::new(header(MediaKind::StreamConfig, 1), config).unwrap(),
                permit(1),
            ))
            .unwrap();
        packets
            .try_send((
                MediaPacket::new(header(MediaKind::Video, 2), vec![0, 0, 0, 1, 0x65, 9]).unwrap(),
                permit(1),
            ))
            .unwrap();
        drop(packets);

        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            decode(router, packet_queue, events, keyframes);
            let _ = done.send(());
        });

        assert!(
            finished
                .recv_timeout(std::time::Duration::from_secs(5))
                .is_ok(),
            "decode loop wedged instead of completing"
        );
        assert!(
            keyframe_queue.try_recv().is_ok(),
            "a decode failure must always leave a keyframe request queued"
        );

        let mut saw_failure = false;
        while let Ok(event) = event_rx.try_recv() {
            if matches!(event, StreamEvent::DecodeFailed { .. }) {
                saw_failure = true;
            }
        }
        assert!(saw_failure, "expected the decode failure to be reported");
    }
}

async fn send_input(sink: &mut Sink, input: MediaInput) -> Result<(), ClientError> {
    let encoded = serde_json::to_string(&input).map_err(ClientError::Encode)?;
    sink.send(Message::Text(encoded.into()))
        .await
        .map_err(|error| ClientError::Send(Box::new(error)))
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("failed to connect to the media endpoint: {0}")]
    Connect(Box<tokio_tungstenite::tungstenite::Error>),
    #[error("server did not negotiate the {MEDIA_WEBSOCKET_SUBPROTOCOL} subprotocol")]
    Subprotocol,
    #[error("failed to encode input: {0}")]
    Encode(serde_json::Error),
    #[error("failed to send input: {0}")]
    Send(Box<tokio_tungstenite::tungstenite::Error>),
    #[error("refusing to send out-of-range input: {0}")]
    InvalidInput(InputValidationError),
    #[error("failed to start the decode thread: {0}")]
    DecodeThread(std::io::Error),
    #[error("input queue is full; the connection is not keeping up")]
    InputBackpressure,
    #[error("the media connection has closed")]
    Disconnected,
}
