use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use navette_protocol::media::{MEDIA_WEBSOCKET_SUBPROTOCOL, MediaInput, MediaPacket};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::router::{DecoderFactory, StreamEvent, StreamRouter};

const EVENT_QUEUE_CAPACITY: usize = 8;
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
        let router = StreamRouter::new(factory);
        let connection = tokio::spawn(run(stream, sink, router, sender));
        Ok(Self { events, connection })
    }

    /// Yields the next decoded frame or stream lifecycle event, or `None` once
    /// the connection has closed.
    pub async fn next_event(&mut self) -> Option<StreamEvent> {
        self.events.recv().await
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
) {
    while let Some(message) = stream.next().await {
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                tracing::debug!(%error, "media WebSocket receive failed");
                break;
            }
        };
        match message {
            Message::Binary(bytes) => {
                let packet = match MediaPacket::decode(&bytes) {
                    Ok(packet) => packet,
                    Err(error) => {
                        tracing::warn!(%error, "discarding malformed media packet");
                        continue;
                    }
                };
                let Some(event) = router.handle(&packet) else {
                    continue;
                };
                if let StreamEvent::DecodeFailed { stream_id } = event {
                    tracing::info!(stream_id, "requesting a keyframe after a decode failure");
                    if send_input(&mut sink, MediaInput::RequestKeyframe)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                if sender.send(event).await.is_err() {
                    break;
                }
            }
            // The server reports protocol problems as JSON text; they are
            // informational and must not take the connection down.
            Message::Text(text) => tracing::warn!(%text, "media server reported an error"),
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
    tracing::debug!("media connection closed");
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
}
