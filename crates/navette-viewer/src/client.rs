use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use navette_protocol::media::{
    InputValidationError, MEDIA_HEADER_LEN, MEDIA_WEBSOCKET_SUBPROTOCOL, MediaInput, MediaKind,
    MediaPacket,
};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
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
const SUBPROTOCOL_HEADER: &str = "Sec-WebSocket-Protocol";

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Sink = SplitSink<Socket, Message>;

/// Client for one session's media endpoint.
///
/// The client owns the socket and a [`StreamRouter`]; decoded frames and
/// stream lifecycle events are handed to the caller through a bounded channel,
/// so a consumer that falls behind exerts backpressure on the socket instead
/// of growing an unbounded backlog. Decoding runs on the connection task and
/// briefly blocks it while FFmpeg works — acceptable for a validation client,
/// which decodes a handful of small streams.
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
        let router = StreamRouter::new(factory);
        let connection = tokio::spawn(run(stream, sink, router, sender, input_queue));
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
    fn drop(&mut self) {
        self.connection.abort();
    }
}

async fn run(
    mut stream: SplitStream<Socket>,
    mut sink: Sink,
    mut router: StreamRouter,
    sender: mpsc::Sender<StreamEvent>,
    mut inputs: mpsc::Receiver<MediaInput>,
) {
    loop {
        tokio::select! {
            message = stream.next() => {
                let Some(message) = message else { break };
                if !receive(message, &mut sink, &mut router, &sender).await {
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

/// Runs blocking work without stalling the rest of the runtime.
///
/// `block_in_place` is only legal on the multi-threaded runtime and panics
/// elsewhere, so the flavour is checked rather than assumed: a caller driving
/// this client from a current-thread runtime (the integration tests do) gets
/// the work run inline, which is correct there because nothing else needs the
/// runtime to stay responsive.
fn without_starving_the_runtime<T>(work: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};

    match Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Ok(RuntimeFlavor::MultiThread) => tokio::task::block_in_place(work),
        _ => work(),
    }
}

/// Routes one received WebSocket message. Returns `false` when the connection
/// must be torn down.
async fn receive(
    message: Result<Message, tokio_tungstenite::tungstenite::Error>,
    sink: &mut Sink,
    router: &mut StreamRouter,
    sender: &mpsc::Sender<StreamEvent>,
) -> bool {
    let message = match message {
        Ok(message) => message,
        Err(error) => {
            tracing::debug!(%error, "media WebSocket receive failed");
            return false;
        }
    };
    match message {
        Message::Binary(bytes) => {
            let packet = match MediaPacket::decode(&bytes) {
                Ok(packet) => packet,
                Err(error) => {
                    tracing::warn!(%error, "discarding malformed media packet");
                    return true;
                }
            };
            // One packet can drain several buffered frames at once (the
            // decode pipeline runs a few access units behind), so every
            // event it produces is forwarded in order rather than at
            // most one.
            // `router.handle` is synchronous and genuinely blocking: it
            // writes to FFmpeg's stdin, reads decoded frames back, and on a
            // stream reconfigure spawns a whole new FFmpeg process and waits
            // for it to prime -- measured at ~600ms against a real session.
            //
            // Running that on a worker does not "starve the runtime" in the
            // obvious sense; this machine has 22 workers and one spawned
            // task. The damage is narrower. Exactly one worker at a time
            // holds tokio's I/O+time driver (it sits behind a `try_lock`,
            // and the others park on a condvar). When the worker holding it
            // stops parking because its task went blocking, nothing re-enters
            // the driver: tokio's eager driver-handoff path is compiled out
            // unless `tokio_unstable` is set, which this workspace does not
            // set. The *time* driver therefore stops being polled and every
            // timer in the process stops firing -- including the `interval`
            // driving the viewer's input poll, and `tokio::signal::ctrl_c`.
            //
            // `block_in_place` hands this worker's core to a fresh thread,
            // which finds no work, parks, and takes the driver -- repairing
            // exactly what broke. It is used rather than `spawn_blocking`
            // because the closure borrows `router` and `packet`, so moving it
            // to another thread is a `'static` problem requiring the router
            // to be owned elsewhere; `Send` is not the obstacle (`Decoder` is
            // already `Send`). That distinction matters: a router-owning task
            // fed by a channel is the shape that would also fix the gap
            // below, and it is available.
            //
            // SCOPE: this restores the *cadence* of input sampling, not the
            // *latency* of input delivery. While this branch is blocked the
            // select's sibling `inputs.recv()` branch is not polled either,
            // so queued input still waits for `router.handle` to return.
            // Closing that needs the router moved off this task entirely.
            //
            // Metrics packets are a no-op in `router.handle`, so they skip
            // the core handoff rather than paying one ~50 times a second.
            let events = if matches!(packet.header.kind, MediaKind::Metrics) {
                router.handle(&packet)
            } else {
                without_starving_the_runtime(|| router.handle(&packet))
            };
            let observation = observe(&packet, router);
            if sender.send(StreamEvent::Packet(observation)).await.is_err() {
                return false;
            }
            for event in events {
                if let StreamEvent::DecodeFailed { stream_id } = event {
                    tracing::info!(stream_id, "requesting a keyframe after a decode failure");
                    if send_input(sink, MediaInput::RequestKeyframe).await.is_err() {
                        return false;
                    }
                }
                if sender.send(event).await.is_err() {
                    return false;
                }
            }
            true
        }
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

/// Snapshots what a packet cost on the wire, plus its stream's decoder
/// counters as they stand *after* it was routed, so a HUD sees this packet's
/// own decode timing.
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
    #[error("input queue is full; the connection is not keeping up")]
    InputBackpressure,
    #[error("the media connection has closed")]
    Disconnected,
}
