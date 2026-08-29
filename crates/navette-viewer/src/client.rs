use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use navette_protocol::media::{
    InputValidationError, MEDIA_HEADER_LEN, MEDIA_WEBSOCKET_SUBPROTOCOL, MediaInput, MediaPacket,
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
        let (packets, packet_queue) = mpsc::channel(PACKET_QUEUE_CAPACITY);
        let router = StreamRouter::new(factory);
        // The decode thread holds a sender for `inputs` so it can ask for a
        // keyframe after a decode failure; that clone also means `inputs`
        // outlives this constructor's local.
        let keyframes = inputs.clone();
        std::thread::Builder::new()
            .name("navette-decode".to_owned())
            .spawn(move || decode(router, packet_queue, sender, keyframes))
            .map_err(ClientError::DecodeThread)?;
        let connection = tokio::spawn(run(stream, sink, packets, input_queue));
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
    packets: mpsc::Sender<MediaPacket>,
    mut inputs: mpsc::Receiver<MediaInput>,
) {
    loop {
        tokio::select! {
            message = stream.next() => {
                let Some(message) = message else { break };
                if !receive(message, &packets).await {
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
    mut packets: mpsc::Receiver<MediaPacket>,
    events: mpsc::Sender<StreamEvent>,
    inputs: mpsc::Sender<MediaInput>,
) {
    while let Some(packet) = packets.blocking_recv() {
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
                // `try_send`, never a blocking send: `inputs` is drained only
                // by the connection task, which may itself be parked handing
                // this thread a packet. Blocking here would close that loop
                // into a deadlock. Dropping the request is self-healing --
                // the decoder is still broken, so the next packet raises
                // `DecodeFailed` again and asks anew.
                if inputs.try_send(MediaInput::RequestKeyframe).is_err() {
                    tracing::warn!(
                        stream_id,
                        "input queue full; deferring the keyframe request to the next failure"
                    );
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
    packets: &mpsc::Sender<MediaPacket>,
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
            Ok(packet) => packets.send(packet).await.is_ok(),
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
