use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response as HttpResponse};
use axum::routing::{any, get, post};
use futures_util::{SinkExt, StreamExt};
use navette_protocol::media::{
    MAX_INPUT_MESSAGE, MEDIA_WEBSOCKET_SUBPROTOCOL, MediaInput, MediaServerMessage,
};
use navette_protocol::{
    API_VERSION, AttachInfo, ErrorCode, Request, RequestCommand, Response, ResponseResult,
    WEBSOCKET_SUBPROTOCOL,
};
use serde_json::{Value, json};

use crate::app_index::AppIndex;
use crate::blobs::{BlobStore, BlobStoreError};
use crate::bridge::BridgeManager;
use crate::media::{MediaHub, MediaHubError};
use crate::registry::{RegistryError, validate_session_name};
use crate::supervisor::{ProcessRunner, Supervisor, SupervisorError};

const MAX_CONTROL_MESSAGE_SIZE: usize = 1024 * 1024;
const MAX_INPUT_MESSAGES_PER_SECOND: u32 = 240;

pub struct ApiState<R: ProcessRunner> {
    pub apps: Arc<AppIndex>,
    pub supervisor: Arc<Supervisor<R>>,
    pub media: MediaHub,
    pub blobs: BlobStore,
    pub bridges: BridgeManager,
    pub auth: Arc<navette_auth::AuthToken>,
}

impl<R: ProcessRunner> Clone for ApiState<R> {
    fn clone(&self) -> Self {
        Self {
            apps: Arc::clone(&self.apps),
            supervisor: Arc::clone(&self.supervisor),
            media: self.media.clone(),
            blobs: self.blobs.clone(),
            bridges: self.bridges.clone(),
            auth: Arc::clone(&self.auth),
        }
    }
}

impl<R: ProcessRunner> ApiState<R> {
    pub fn new(
        apps: Arc<AppIndex>,
        supervisor: Arc<Supervisor<R>>,
        auth: Arc<navette_auth::AuthToken>,
    ) -> Self {
        let media = MediaHub::default();
        let blobs = BlobStore::new(supervisor.blob_root());
        Self {
            apps,
            supervisor,
            bridges: BridgeManager::new(media.clone(), blobs.clone()),
            media,
            blobs,
            auth,
        }
    }

    pub fn start_existing_bridges(&self) {
        let sessions = self
            .supervisor
            .registry()
            .lock()
            .map(|registry| registry.list())
            .unwrap_or_default();
        for session in sessions {
            if matches!(session.status, navette_protocol::SessionStatus::Running)
                && let Err(error) = self.bridges.start(&session)
            {
                tracing::error!(session = %session.name, %error, "failed to resume session bridge");
            }
        }
    }
}

pub fn router<R: ProcessRunner>(state: ApiState<R>) -> Router {
    let auth = Arc::clone(&state.auth);
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/ws", any(websocket::<R>))
        .route("/v1/sessions/{session}/media", any(media_websocket::<R>))
        .route("/v1/sessions/{session}/blobs", post(upload_blob::<R>))
        .route("/v1/sessions/{session}/blobs/{id}", get(download_blob::<R>))
        .layer(axum::middleware::from_fn_with_state(
            auth,
            crate::guard::authenticate,
        ))
        .layer(axum::middleware::from_fn(
            crate::guard::reject_browser_origin,
        ))
        .with_state(state)
}

async fn upload_blob<R: ProcessRunner>(
    Path(session): Path<String>,
    State(state): State<ApiState<R>>,
    headers: HeaderMap,
    body: Body,
) -> HttpResponse {
    let Some(mime) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    };
    if mime.contains(';') {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }

    let mut writer = match state.blobs.begin_write(&session, mime) {
        Ok(writer) => writer,
        Err(error) => return blob_error_response(error),
    };
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        };
        if let Err(error) = writer.write_chunk(&chunk) {
            return blob_error_response(error);
        }
    }
    match writer.finish() {
        Ok(blob) => (StatusCode::CREATED, Json(blob)).into_response(),
        Err(error) => blob_error_response(error),
    }
}

async fn download_blob<R: ProcessRunner>(
    Path((session, id)): Path<(String, String)>,
    State(state): State<ApiState<R>>,
) -> HttpResponse {
    let placeholder = navette_protocol::media::BlobDescriptor {
        id,
        mime: "image/png".into(),
        size: 1,
    };
    if placeholder.validate().is_err() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(blob) = state.blobs.descriptor(&session, &placeholder.id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match state.blobs.read(&session, &blob) {
        Ok(bytes) => {
            let mut response = Body::from(bytes).into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                blob.mime.parse().expect("BlobDescriptor validates MIME"),
            );
            response
        }
        Err(error) => blob_error_response(error),
    }
}

fn blob_error_response(error: BlobStoreError) -> HttpResponse {
    match error {
        BlobStoreError::UnsupportedMime => StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response(),
        BlobStoreError::BlobTooLarge => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        BlobStoreError::SessionBudgetExceeded => StatusCode::INSUFFICIENT_STORAGE.into_response(),
        BlobStoreError::InvalidSession
        | BlobStoreError::InvalidDescriptor
        | BlobStoreError::NotFound => StatusCode::NOT_FOUND.into_response(),
        BlobStoreError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use tempfile::TempDir;

    use super::*;
    use navette_auth::AuthToken;

    /// Leaks a TempDir so the returned router owns a live registry path for the
    /// duration of the test. Acceptable in tests; never do this in production
    /// code.
    pub(crate) fn test_router() -> Router {
        test_router_with_token().0
    }

    pub(crate) fn test_router_with_token() -> (Router, Arc<AuthToken>) {
        let temp = Box::leak(Box::new(TempDir::new().unwrap()));
        let state = super::tests::test_state(temp);
        let token = Arc::clone(&state.auth);
        (router(state), token)
    }
}

async fn media_websocket<R: ProcessRunner>(
    ws: WebSocketUpgrade,
    Path(session): Path<String>,
    State(state): State<ApiState<R>>,
) -> HttpResponse {
    if validate_session_name(&session).is_err() {
        return (StatusCode::BAD_REQUEST, "invalid session name").into_response();
    }
    let exists = state
        .supervisor
        .registry()
        .lock()
        .ok()
        .and_then(|registry| registry.get(&session).cloned())
        .is_some_and(|record| {
            matches!(
                record.status,
                navette_protocol::SessionStatus::Starting
                    | navette_protocol::SessionStatus::Running
            )
        });
    if !exists {
        return (StatusCode::NOT_FOUND, "session is not running").into_response();
    }
    if !ws
        .requested_protocols()
        .any(|protocol| protocol.as_bytes() == MEDIA_WEBSOCKET_SUBPROTOCOL.as_bytes())
    {
        return (
            StatusCode::BAD_REQUEST,
            format!("WebSocket subprotocol {MEDIA_WEBSOCKET_SUBPROTOCOL} is required"),
        )
            .into_response();
    }
    let Ok(attachment) = state.media.attach(&session) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "session media is not ready",
        )
            .into_response();
    };

    ws.protocols([MEDIA_WEBSOCKET_SUBPROTOCOL])
        .max_message_size(MAX_INPUT_MESSAGE * 2)
        .on_upgrade(move |socket| handle_media_socket(socket, attachment))
}

async fn handle_media_socket(socket: WebSocket, attachment: crate::media::MediaAttachment) {
    let (mut sender, mut receiver) = socket.split();
    let mut rate_window = std::time::Instant::now();
    let mut rate_count = 0_u32;
    loop {
        tokio::select! {
            packet = attachment.recv() => {
                let Some(packet) = packet else {
                    break;
                };
                let Ok(encoded) = packet.encode() else {
                    tracing::error!("media hub produced an invalid packet");
                    break;
                };
                if sender.send(Message::Binary(encoded.into())).await.is_err() {
                    break;
                }
            }
            message = attachment.recv_message() => {
                let Some(message) = message else { break; };
                let Ok(encoded) = serde_json::to_string(&message) else { break; };
                if sender.send(Message::Text(encoded.into())).await.is_err() {
                    break;
                }
            }
            incoming = receiver.next() => {
                let Some(incoming) = incoming else {
                    break;
                };
                let error = match incoming {
                    Ok(Message::Text(text)) => {
                        if text.len() > MAX_INPUT_MESSAGE {
                            let response = MediaServerMessage::Error {
                                code: "message_too_large".to_string(),
                                message: format!(
                                    "input message exceeds {MAX_INPUT_MESSAGE} bytes"
                                ),
                            };
                            let Ok(encoded) = serde_json::to_string(&response) else {
                                break;
                            };
                            if sender.send(Message::Text(encoded.into())).await.is_err() {
                                break;
                            }
                            continue;
                        }
                        if rate_window.elapsed() >= std::time::Duration::from_secs(1) {
                            rate_window = std::time::Instant::now();
                            rate_count = 0;
                        }
                        rate_count = rate_count.saturating_add(1);
                        if rate_count > MAX_INPUT_MESSAGES_PER_SECOND {
                            Some(("rate_limited", "input rate limit exceeded".to_string()))
                        } else {
                            match serde_json::from_str::<MediaInput>(&text) {
                                // Answered here rather than forwarded: the
                                // bridge has no JSON path back to a client
                                // (MediaAttachment::recv yields packets only),
                                // and a pong that queued behind the bridge
                                // loop would measure the loop, not the link.
                                Ok(MediaInput::Ping { nonce }) => {
                                    let pong = MediaServerMessage::Pong { nonce };
                                    let Ok(encoded) = serde_json::to_string(&pong) else {
                                        break;
                                    };
                                    if sender.send(Message::Text(encoded.into())).await.is_err() {
                                        break;
                                    }
                                    None
                                }
                                Ok(input) => attachment.submit_input(input).err().map(media_hub_error),
                                Err(error) => Some(("invalid_input", format!("invalid input: {error}"))),
                            }
                        }
                    }
                    Ok(Message::Binary(_)) => Some((
                        "invalid_input",
                        "client media messages must be JSON text".to_string(),
                    )),
                    Ok(Message::Close(_)) => break,
                    Ok(Message::Ping(_) | Message::Pong(_)) => None,
                    Err(error) => {
                        tracing::debug!(%error, "media WebSocket receive failed");
                        break;
                    }
                };
                if let Some((code, message)) = error {
                    let response = MediaServerMessage::Error {
                        code: code.to_string(),
                        message,
                    };
                    let Ok(encoded) = serde_json::to_string(&response) else {
                        break;
                    };
                    if sender.send(Message::Text(encoded.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
}

fn media_hub_error(error: MediaHubError) -> (&'static str, String) {
    let code = match error {
        MediaHubError::InvalidInput(_) => "invalid_input",
        MediaHubError::InputBackpressure => "rate_limited",
        MediaHubError::UnknownSession(_)
        | MediaHubError::InvalidPacket(_)
        | MediaHubError::MissingConfig
        | MediaHubError::NonMonotonicSequence
        | MediaHubError::InvalidDimensions
        | MediaHubError::Unavailable => "unavailable",
    };
    (code, error.to_string())
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok", "api_version": API_VERSION}))
}

async fn websocket<R: ProcessRunner>(
    ws: WebSocketUpgrade,
    State(state): State<ApiState<R>>,
) -> HttpResponse {
    if !ws
        .requested_protocols()
        .any(|protocol| protocol.as_bytes() == WEBSOCKET_SUBPROTOCOL.as_bytes())
    {
        return (
            StatusCode::BAD_REQUEST,
            format!("WebSocket subprotocol {WEBSOCKET_SUBPROTOCOL} is required"),
        )
            .into_response();
    }

    ws.protocols([WEBSOCKET_SUBPROTOCOL])
        .max_message_size(MAX_CONTROL_MESSAGE_SIZE)
        .on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket<R: ProcessRunner>(socket: WebSocket, state: ApiState<R>) {
    let (mut sender, mut receiver) = socket.split();
    while let Some(message) = receiver.next().await {
        let response = match message {
            Ok(Message::Text(text)) => match serde_json::from_str::<Request>(&text) {
                Ok(request) => dispatch(&state, request).await,
                Err(error) => Response::error(
                    extract_request_id(&text),
                    ErrorCode::InvalidRequest,
                    format!("invalid request: {error}"),
                ),
            },
            Ok(Message::Binary(_)) => Response::error(
                0,
                ErrorCode::InvalidRequest,
                "binary control frames are reserved for a future API version",
            ),
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            Err(error) => {
                tracing::debug!(%error, "WebSocket receive failed");
                break;
            }
        };

        let Ok(encoded) = serde_json::to_string(&response) else {
            tracing::error!("failed to serialize API response");
            break;
        };
        if sender.send(Message::Text(encoded.into())).await.is_err() {
            break;
        }
    }
}

pub async fn dispatch<R: ProcessRunner>(state: &ApiState<R>, request: Request) -> Response {
    let request_id = request.request_id;
    let result = match request.command {
        RequestCommand::ListApps => Ok(ResponseResult::Apps {
            apps: state.apps.list(),
        }),
        RequestCommand::ListSessions => state
            .supervisor
            .registry()
            .lock()
            .map(|registry| ResponseResult::Sessions {
                sessions: registry.list(),
            })
            .map_err(|_| ApiFailure::internal("session registry lock is poisoned")),
        RequestCommand::Run { app_id, name } => {
            let Some(app) = state.apps.get(&app_id) else {
                return Response::error(
                    request_id,
                    ErrorCode::NotFound,
                    format!("application not found: {app_id}"),
                );
            };
            let result = state.supervisor.start(app, name.as_deref()).await;
            match result {
                Ok(session) => match state.bridges.start(&session) {
                    Ok(()) => Ok(ResponseResult::Session { session }),
                    Err(error) => {
                        let _ = state.supervisor.kill(&session.name).await;
                        Err(ApiFailure::internal(format!(
                            "failed to start media bridge: {error}"
                        )))
                    }
                },
                Err(error) => Err(ApiFailure::from(error)),
            }
        }
        RequestCommand::Kill { session } => {
            state.bridges.stop(&session);
            state
                .supervisor
                .kill(&session)
                .await
                .map(|_| ResponseResult::Ack)
                .map_err(ApiFailure::from)
        }
        RequestCommand::Attach { session } => {
            if let Err(error) = validate_session_name(&session) {
                Err(ApiFailure::from(error))
            } else {
                let now = now_ms();
                state
                    .supervisor
                    .registry()
                    .lock()
                    .map_err(|_| ApiFailure::internal("session registry lock is poisoned"))
                    .and_then(|mut registry| {
                        registry
                            .mark_attached(&session, now)
                            .map_err(ApiFailure::from)
                    })
                    .map(|record| ResponseResult::Attach {
                        attach: AttachInfo {
                            session: record.name,
                            socket_path: record.socket_path,
                        },
                    })
            }
        }
        RequestCommand::Detach { session } => state
            .supervisor
            .registry()
            .lock()
            .map_err(|_| ApiFailure::internal("session registry lock is poisoned"))
            .and_then(|mut registry| registry.mark_detached(&session).map_err(ApiFailure::from))
            .map(|_| ResponseResult::Ack),
    };

    match result {
        Ok(result) => Response::ok(request_id, result),
        Err(error) => Response::error(request_id, error.code, error.message),
    }
}

fn extract_request_id(text: &str) -> u64 {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| value.get("request_id").and_then(Value::as_u64))
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[derive(Debug)]
struct ApiFailure {
    code: ErrorCode,
    message: String,
}

impl ApiFailure {
    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::Internal,
            message: message.into(),
        }
    }
}

impl From<RegistryError> for ApiFailure {
    fn from(error: RegistryError) -> Self {
        let code = match error {
            RegistryError::InvalidName(_) => ErrorCode::InvalidName,
            RegistryError::AlreadyExists(_) => ErrorCode::AlreadyExists,
            RegistryError::NotFound(_) => ErrorCode::NotFound,
            RegistryError::Read { .. }
            | RegistryError::Decode { .. }
            | RegistryError::UnsupportedVersion(_)
            | RegistryError::Persist { .. } => ErrorCode::Internal,
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
}

impl From<SupervisorError> for ApiFailure {
    fn from(error: SupervisorError) -> Self {
        let code = match &error {
            SupervisorError::Registry(RegistryError::InvalidName(_)) => ErrorCode::InvalidName,
            SupervisorError::Registry(RegistryError::AlreadyExists(_)) => ErrorCode::AlreadyExists,
            SupervisorError::Registry(RegistryError::NotFound(_)) => ErrorCode::NotFound,
            SupervisorError::InvalidApp(_) => ErrorCode::Unavailable,
            SupervisorError::Spawn { .. }
            | SupervisorError::ReadinessTimeout
            | SupervisorError::Terminate { .. } => ErrorCode::ProcessFailed,
            SupervisorError::Registry(_)
            | SupervisorError::RuntimeDirectory { .. }
            | SupervisorError::Clock
            | SupervisorError::RegistryLock => ErrorCode::Internal,
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io;
    use std::sync::Mutex;
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use navette_protocol::media::{MediaFlags, MediaHeader, MediaKind, MediaPacket};
    use navette_protocol::{App, ResponseOutcome, Session, SessionStatus};
    use tempfile::TempDir;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tower::ServiceExt;

    use super::*;
    use crate::registry::Registry;
    use crate::supervisor::{ProcessSpec, Supervisor};

    #[derive(Debug, Default)]
    pub(crate) struct NoopRunner {
        alive: Mutex<BTreeSet<u32>>,
    }

    impl ProcessRunner for NoopRunner {
        fn spawn(&self, _spec: ProcessSpec) -> io::Result<u32> {
            Err(io::Error::other("spawn is not used by this test"))
        }

        fn is_alive(&self, pid: u32) -> bool {
            self.alive.lock().unwrap().contains(&pid)
        }

        fn terminate(&self, _pid: u32, _force: bool) -> io::Result<()> {
            Ok(())
        }
    }

    pub(crate) fn test_state(temp: &TempDir) -> ApiState<NoopRunner> {
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
        ApiState::new(
            Arc::new(apps),
            Arc::new(supervisor),
            Arc::new(navette_auth::AuthToken::generate()),
        )
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

    fn media_packet(kind: MediaKind, sequence: u64, keyframe: bool) -> MediaPacket {
        MediaPacket::new(
            MediaHeader {
                kind,
                flags: MediaFlags::new(keyframe, false),
                stream_id: 1,
                sequence,
                timestamp_us: sequence,
                payload_len: 0,
                width: 1280,
                height: 720,
            },
            vec![sequence as u8],
        )
        .unwrap()
    }

    #[tokio::test]
    async fn unknown_app_is_structured_not_found() {
        let temp = TempDir::new().unwrap();
        let response = dispatch(
            &test_state(&temp),
            Request {
                request_id: 7,
                command: RequestCommand::Run {
                    app_id: "missing".into(),
                    name: None,
                },
            },
        )
        .await;
        assert!(matches!(
            response.outcome,
            ResponseOutcome::Error {
                error: navette_protocol::ApiError {
                    code: ErrorCode::NotFound,
                    ..
                }
            }
        ));
    }

    #[tokio::test]
    async fn websocket_lists_apps_and_echoes_request_id() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        let token = state.auth.render();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });

        let mut request = format!("ws://{address}/v1/ws")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            WEBSOCKET_SUBPROTOCOL.parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse().unwrap());
        let (mut socket, response) = connect_async(request).await.unwrap();
        assert_eq!(
            response.headers()["Sec-WebSocket-Protocol"],
            WEBSOCKET_SUBPROTOCOL
        );
        socket
            .send(ClientMessage::Text(
                serde_json::to_string(&Request {
                    request_id: 99,
                    command: RequestCommand::ListApps,
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let message = socket.next().await.unwrap().unwrap();
        let response: Response = serde_json::from_str(message.to_text().unwrap()).unwrap();
        assert_eq!(response.request_id, 99);
        assert!(matches!(
            response.outcome,
            ResponseOutcome::Ok {
                result: ResponseResult::Apps { apps }
            } if apps.len() == 1 && apps[0].id == "firefox"
        ));
        server.abort();
    }

    #[tokio::test]
    async fn websocket_requires_v1_subprotocol() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        let token = state.auth.render();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });

        // A valid Authorization header but no subprotocol: a 400 here (not a
        // 401) is what proves this rejection is about the missing
        // subprotocol, not the credential.
        let mut request = format!("ws://{address}/v1/ws")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse().unwrap());
        let error = connect_async(request).await.unwrap_err();
        assert!(error.to_string().contains("400"));
        server.abort();
    }

    #[tokio::test]
    async fn media_websocket_replays_bootstrap_and_routes_validated_input() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        add_running_session(&state, "work");
        let mut input = state.media.register_session("work");
        state
            .media
            .publish("work", media_packet(MediaKind::StreamConfig, 1, false))
            .unwrap();
        state
            .media
            .publish("work", media_packet(MediaKind::Video, 2, true))
            .unwrap();

        let token = state.auth.render();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        let mut request = format!("ws://{address}/v1/sessions/work/media")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            MEDIA_WEBSOCKET_SUBPROTOCOL.parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse().unwrap());
        let (mut socket, response) = connect_async(request).await.unwrap();
        assert_eq!(
            response.headers()["Sec-WebSocket-Protocol"],
            MEDIA_WEBSOCKET_SUBPROTOCOL
        );
        let config = socket.next().await.unwrap().unwrap().into_data();
        let keyframe = socket.next().await.unwrap().unwrap().into_data();
        assert_eq!(
            MediaPacket::decode(&config).unwrap().header.kind,
            MediaKind::StreamConfig
        );
        assert!(
            MediaPacket::decode(&keyframe)
                .unwrap()
                .header
                .flags
                .keyframe()
        );

        socket
            .send(ClientMessage::Text(
                r#"{"type":"viewport_resize","width":10,"height":10}"#.into(),
            ))
            .await
            .unwrap();
        let error = socket.next().await.unwrap().unwrap();
        assert!(error.to_text().unwrap().contains("invalid_input"));
        socket
            .send(ClientMessage::Text(r#"{"type":"request_keyframe"}"#.into()))
            .await
            .unwrap();
        assert_eq!(
            input.recv().await,
            Some(crate::media::MediaCommand::Input {
                attachment_id: 1,
                input: MediaInput::RequestKeyframe,
                // Ignored by `PartialEq`; any instant will do.
                queued_at: std::time::Instant::now()
            })
        );
        server.abort();
    }

    #[tokio::test]
    async fn media_websocket_answers_a_ping_without_troubling_the_bridge() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        add_running_session(&state, "work");
        let mut input = state.media.register_session("work");

        let token = state.auth.render();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        let mut request = format!("ws://{address}/v1/sessions/work/media")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            MEDIA_WEBSOCKET_SUBPROTOCOL.parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse().unwrap());
        let (mut socket, _response) = connect_async(request).await.unwrap();

        socket
            .send(ClientMessage::Text(r#"{"type":"ping","nonce":99}"#.into()))
            .await
            .unwrap();
        let pong = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("timed out waiting for a pong -- the server did not answer the ping")
            .unwrap()
            .unwrap();
        assert_eq!(pong.to_text().unwrap(), r#"{"type":"pong","nonce":99}"#);

        // The bridge must never see a ping: it is answered at the socket, so a
        // stalled bridge loop cannot delay it -- and equally cannot be measured
        // by it. Sending a real input afterwards proves the channel still works
        // and that nothing from the ping is sitting ahead of it in the queue.
        socket
            .send(ClientMessage::Text(r#"{"type":"request_keyframe"}"#.into()))
            .await
            .unwrap();
        let forwarded = tokio::time::timeout(std::time::Duration::from_secs(5), input.recv())
            .await
            .expect("timed out waiting for the forwarded request_keyframe");
        assert_eq!(
            forwarded,
            Some(crate::media::MediaCommand::Input {
                attachment_id: 1,
                input: MediaInput::RequestKeyframe,
                queued_at: std::time::Instant::now()
            })
        );
        server.abort();
    }

    #[tokio::test]
    async fn published_server_messages_reach_the_socket_as_text() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        add_running_session(&state, "work");
        let hub = state.media.clone();
        let _input = state.media.register_session("work");
        let token = state.auth.render();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        let mut request = format!("ws://{address}/v1/sessions/work/media")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            MEDIA_WEBSOCKET_SUBPROTOCOL.parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse().unwrap());
        let (mut socket, _response) = connect_async(request).await.unwrap();

        hub.publish_message(
            "work",
            MediaServerMessage::Clipboard {
                text: "hello".into(),
            },
        );

        let received = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("a clipboard message should arrive before the timeout")
            .expect("socket should stay open")
            .unwrap();

        assert_eq!(
            received,
            ClientMessage::Text(r#"{"type":"clipboard","text":"hello"}"#.into())
        );
        server.abort();
    }

    #[tokio::test]
    async fn our_own_websocket_client_sends_no_origin_header() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        let token = state.auth.render();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        let mut request = format!("ws://{address}/v1/ws")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            WEBSOCKET_SUBPROTOCOL.parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse().unwrap());
        assert!(
            !request.headers().contains_key("Origin"),
            "our client must not send Origin, or the guard would lock us out"
        );
        assert!(connect_async(request).await.is_ok());
    }

    #[test]
    fn malformed_request_preserves_numeric_request_id() {
        assert_eq!(
            extract_request_id(r#"{"request_id": 42, "type": "run"}"#),
            42
        );
        assert_eq!(extract_request_id("not json"), 0);
    }

    #[tokio::test]
    async fn blob_routes_require_auth_reject_bad_mime_and_round_trip_a_session_blob() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        let token = state.auth.render();
        let app = router(state);

        let unauthenticated = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/sessions/work/blobs")
            .header("content-type", "image/png")
            .body(axum::body::Body::from("png"))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(unauthenticated).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let unsupported = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/sessions/work/blobs")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "image/svg+xml")
            .body(axum::body::Body::from("svg"))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(unsupported).await.unwrap().status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );

        let upload = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/sessions/work/blobs")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "image/png")
            .body(axum::body::Body::from("png"))
            .unwrap();
        let response = app.clone().oneshot(upload).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let blob: navette_protocol::media::BlobDescriptor = serde_json::from_slice(&body).unwrap();

        let download = axum::http::Request::builder()
            .uri(format!("/v1/sessions/work/blobs/{}", blob.id))
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(download).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "image/png");
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap(),
            "png"
        );
    }

    #[tokio::test]
    async fn blob_routes_return_not_found_payload_too_large_and_insufficient_storage() {
        let temp = TempDir::new().unwrap();
        let mut state = test_state(&temp);
        state.blobs = BlobStore::with_limits(temp.path().join("limited-blobs"), 4, 6);
        let token = state.auth.render();
        let app = router(state);

        let missing = axum::http::Request::builder()
            .uri("/v1/sessions/work/blobs/0123456789abcdef0123456789abcdef")
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(missing).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );

        let too_large = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/sessions/work/blobs")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "image/png")
            .body(axum::body::Body::from("12345"))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(too_large).await.unwrap().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );

        let first = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/sessions/work/blobs")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "image/png")
            .body(axum::body::Body::from("1234"))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(first).await.unwrap().status(),
            StatusCode::CREATED
        );
        let exhausted = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/sessions/work/blobs")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "image/png")
            .body(axum::body::Body::from("123"))
            .unwrap();
        assert_eq!(
            app.oneshot(exhausted).await.unwrap().status(),
            StatusCode::INSUFFICIENT_STORAGE
        );
    }
}
