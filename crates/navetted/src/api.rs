use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as HttpResponse};
use axum::routing::{any, get};
use futures_util::{SinkExt, StreamExt};
use navette_protocol::{
    API_VERSION, AttachInfo, ErrorCode, Request, RequestCommand, Response, ResponseResult,
    WEBSOCKET_SUBPROTOCOL,
};
use serde_json::{Value, json};

use crate::app_index::AppIndex;
use crate::registry::{RegistryError, validate_session_name};
use crate::supervisor::{ProcessRunner, Supervisor, SupervisorError};

const MAX_CONTROL_MESSAGE_SIZE: usize = 1024 * 1024;

pub struct ApiState<R: ProcessRunner> {
    pub apps: Arc<AppIndex>,
    pub supervisor: Arc<Supervisor<R>>,
    clipboard: Arc<Mutex<Option<String>>>,
}

impl<R: ProcessRunner> Clone for ApiState<R> {
    fn clone(&self) -> Self {
        Self {
            apps: Arc::clone(&self.apps),
            supervisor: Arc::clone(&self.supervisor),
            clipboard: Arc::clone(&self.clipboard),
        }
    }
}

impl<R: ProcessRunner> ApiState<R> {
    pub fn new(apps: Arc<AppIndex>, supervisor: Arc<Supervisor<R>>) -> Self {
        Self {
            apps,
            supervisor,
            clipboard: Arc::new(Mutex::new(None)),
        }
    }
}

pub fn router<R: ProcessRunner>(state: ApiState<R>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/ws", any(websocket::<R>))
        .with_state(state)
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
            state
                .supervisor
                .start(app, name.as_deref())
                .await
                .map(|session| ResponseResult::Session { session })
                .map_err(ApiFailure::from)
        }
        RequestCommand::Kill { session } => state
            .supervisor
            .kill(&session)
            .await
            .map(|_| ResponseResult::Ack)
            .map_err(ApiFailure::from),
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
        RequestCommand::SetClipboard { text } => state
            .clipboard
            .lock()
            .map_err(|_| ApiFailure::internal("clipboard lock is poisoned"))
            .map(|mut clipboard| {
                *clipboard = Some(text);
                ResponseResult::Ack
            }),
        RequestCommand::GetClipboard => state
            .clipboard
            .lock()
            .map_err(|_| ApiFailure::internal("clipboard lock is poisoned"))
            .map(|clipboard| ResponseResult::Clipboard {
                text: clipboard.clone(),
            }),
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
    use navette_protocol::{App, ResponseOutcome};
    use tempfile::TempDir;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    use super::*;
    use crate::registry::Registry;
    use crate::supervisor::{ProcessSpec, Supervisor};

    #[derive(Debug, Default)]
    struct NoopRunner {
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

    #[tokio::test]
    async fn clipboard_round_trip() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        let set = dispatch(
            &state,
            Request {
                request_id: 1,
                command: RequestCommand::SetClipboard {
                    text: "hello".into(),
                },
            },
        )
        .await;
        assert!(matches!(
            set.outcome,
            ResponseOutcome::Ok {
                result: ResponseResult::Ack
            }
        ));

        let get = dispatch(
            &state,
            Request {
                request_id: 2,
                command: RequestCommand::GetClipboard,
            },
        )
        .await;
        assert!(matches!(
            get.outcome,
            ResponseOutcome::Ok {
                result: ResponseResult::Clipboard { text: Some(text) }
            } if text == "hello"
        ));
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(test_state(&temp)))
                .await
                .unwrap();
        });

        let mut request = format!("ws://{address}/v1/ws")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            WEBSOCKET_SUBPROTOCOL.parse().unwrap(),
        );
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(test_state(&temp)))
                .await
                .unwrap();
        });

        let error = connect_async(format!("ws://{address}/v1/ws"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("400"));
        server.abort();
    }

    #[test]
    fn malformed_request_preserves_numeric_request_id() {
        assert_eq!(
            extract_request_id(r#"{"request_id": 42, "type": "run"}"#),
            42
        );
        assert_eq!(extract_request_id("not json"), 0);
    }
}
