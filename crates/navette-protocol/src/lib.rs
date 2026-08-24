//! Stable request/response types for the Navette v1 control-channel API.
//!
//! The v1 control channel uses one JSON value per WebSocket text frame. Binary
//! frames are reserved for media channels introduced after M1.

use serde::{Deserialize, Serialize};

/// WebSocket subprotocol negotiated by v1 clients and servers.
pub const WEBSOCKET_SUBPROTOCOL: &str = "navette.v1";

/// Current major API version.
pub const API_VERSION: u16 = 1;

pub type RequestId = u64;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Request {
    pub request_id: RequestId,
    #[serde(flatten)]
    pub command: RequestCommand,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestCommand {
    ListApps,
    ListSessions,
    Run {
        app_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Kill {
        session: String,
    },
    Attach {
        session: String,
    },
    Detach {
        session: String,
    },
    SetClipboard {
        text: String,
    },
    GetClipboard,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Response {
    pub request_id: RequestId,
    #[serde(flatten)]
    pub outcome: ResponseOutcome,
}

impl Response {
    pub fn ok(request_id: RequestId, result: ResponseResult) -> Self {
        Self {
            request_id,
            outcome: ResponseOutcome::Ok { result },
        }
    }

    pub fn error(request_id: RequestId, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            request_id,
            outcome: ResponseOutcome::Error {
                error: ApiError {
                    code,
                    message: message.into(),
                },
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResponseOutcome {
    Ok {
        #[serde(flatten)]
        result: ResponseResult,
    },
    Error {
        error: ApiError,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseResult {
    Apps {
        apps: Vec<App>,
    },
    Sessions {
        sessions: Vec<Session>,
    },
    Session {
        session: Session,
    },
    Attach {
        attach: AttachInfo,
    },
    Ack,
    Clipboard {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct App {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    pub exec: Vec<String>,
    #[serde(default)]
    pub terminal: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Session {
    pub name: String,
    pub app_id: String,
    pub app_pid: u32,
    pub daemon_pid: u32,
    pub wayland_display: String,
    pub socket_path: String,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attached_at_ms: Option<u64>,
    #[serde(default)]
    pub client_count: u32,
    pub status: SessionStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Running,
    Failed,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct AttachInfo {
    pub session: String,
    pub socket_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    NotFound,
    AlreadyExists,
    InvalidName,
    ProcessFailed,
    Unavailable,
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_fixtures_round_trip() {
        let fixtures = [
            json!({"request_id": 1, "type": "list_apps"}),
            json!({"request_id": 2, "type": "list_sessions"}),
            json!({"request_id": 3, "type": "run", "app_id": "firefox.desktop"}),
            json!({"request_id": 4, "type": "run", "app_id": "firefox.desktop", "name": "work"}),
            json!({"request_id": 5, "type": "kill", "session": "work"}),
            json!({"request_id": 6, "type": "attach", "session": "work"}),
            json!({"request_id": 7, "type": "detach", "session": "work"}),
            json!({"request_id": 8, "type": "set_clipboard", "text": "hello"}),
            json!({"request_id": 9, "type": "get_clipboard"}),
        ];

        for fixture in fixtures {
            let request: Request = serde_json::from_value(fixture.clone()).unwrap();
            assert_eq!(serde_json::to_value(request).unwrap(), fixture);
        }
    }

    #[test]
    fn response_fixtures_round_trip() {
        let fixtures = [
            json!({"request_id": 1, "status": "ok", "type": "apps", "apps": []}),
            json!({"request_id": 2, "status": "ok", "type": "sessions", "sessions": []}),
            json!({"request_id": 3, "status": "ok", "type": "ack"}),
            json!({"request_id": 4, "status": "ok", "type": "clipboard"}),
            json!({
                "request_id": 5,
                "status": "error",
                "error": {"code": "not_found", "message": "missing"}
            }),
        ];

        for fixture in fixtures {
            let response: Response = serde_json::from_value(fixture.clone()).unwrap();
            assert_eq!(serde_json::to_value(response).unwrap(), fixture);
        }
    }

    #[test]
    fn unknown_fields_are_accepted_for_additive_evolution() {
        let value = json!({
            "request_id": 1,
            "type": "list_apps",
            "future_optional_field": true
        });
        let request: Request = serde_json::from_value(value).unwrap();
        assert_eq!(request.command, RequestCommand::ListApps);
    }

    #[test]
    fn session_fixture_uses_unambiguous_wire_names() {
        let session = Session {
            name: "work".into(),
            app_id: "firefox.desktop".into(),
            app_pid: 10,
            daemon_pid: 11,
            wayland_display: "navette-work".into(),
            socket_path: "/run/user/1000/navette/work/wprs.sock".into(),
            created_at_ms: 1_700_000_000_000,
            last_attached_at_ms: Some(1_700_000_001_000),
            client_count: 1,
            status: SessionStatus::Running,
        };
        let value = serde_json::to_value(Response::ok(
            42,
            ResponseResult::Session {
                session: session.clone(),
            },
        ))
        .unwrap();

        assert_eq!(value["status"], "ok");
        assert_eq!(value["type"], "session");
        assert_eq!(value["session"]["status"], "running");
        let decoded: Response = serde_json::from_value(value).unwrap();
        assert_eq!(
            decoded,
            Response::ok(42, ResponseResult::Session { session })
        );
    }

    #[test]
    fn malformed_command_is_rejected() {
        let result = serde_json::from_value::<Request>(json!({
            "request_id": 1,
            "type": "run"
        }));
        assert!(result.is_err());
    }

    #[test]
    fn constructors_build_expected_outcomes() {
        assert!(matches!(
            Response::ok(1, ResponseResult::Ack).outcome,
            ResponseOutcome::Ok {
                result: ResponseResult::Ack
            }
        ));
        assert!(matches!(
            Response::error(2, ErrorCode::Unavailable, "offline").outcome,
            ResponseOutcome::Error {
                error: ApiError {
                    code: ErrorCode::Unavailable,
                    ..
                }
            }
        ));
    }

    #[test]
    fn api_constants_are_stable() {
        assert_eq!(API_VERSION, 1);
        assert_eq!(WEBSOCKET_SUBPROTOCOL, "navette.v1");
    }
}
