use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use navette_protocol::{
    Request, RequestCommand, Response, ResponseOutcome, ResponseResult, WEBSOCKET_SUBPROTOCOL,
};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[derive(Clone, Debug)]
pub struct Client {
    url: String,
    token: String,
}

impl Client {
    pub fn new(url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            token: token.into(),
        }
    }

    pub async fn call(&self, command: RequestCommand) -> Result<ResponseResult> {
        let mut upgrade = self
            .url
            .as_str()
            .into_client_request()
            .with_context(|| format!("invalid daemon URL: {}", self.url))?;
        upgrade.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            WEBSOCKET_SUBPROTOCOL
                .parse()
                .expect("static subprotocol is a valid header value"),
        );
        upgrade.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", self.token)
                .parse()
                .context("token is not a valid header value")?,
        );
        let (mut socket, response) = connect_async(upgrade)
            .await
            .with_context(|| format!("failed to connect to {}", self.url))?;
        if response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|value| value.to_str().ok())
            != Some(WEBSOCKET_SUBPROTOCOL)
        {
            bail!("daemon did not negotiate {WEBSOCKET_SUBPROTOCOL}");
        }

        let request = Request {
            request_id: 1,
            command,
        };
        socket
            .send(Message::Text(serde_json::to_string(&request)?.into()))
            .await
            .context("failed to send request")?;
        while let Some(message) = socket.next().await {
            let message = message.context("failed to receive daemon response")?;
            if let Message::Text(text) = message {
                let response: Response =
                    serde_json::from_str(&text).context("daemon returned invalid JSON")?;
                if response.request_id != request.request_id {
                    bail!(
                        "daemon response ID {} does not match request ID {}",
                        response.request_id,
                        request.request_id
                    );
                }
                return response_result(response);
            }
        }
        bail!("daemon closed the connection without a response")
    }
}

pub fn response_result(response: Response) -> Result<ResponseResult> {
    match response.outcome {
        ResponseOutcome::Ok { result } => Ok(result),
        ResponseOutcome::Error { error } => {
            bail!("daemon error ({:?}): {}", error.code, error.message)
        }
    }
}

pub fn render_result(result: &ResponseResult) -> String {
    match result {
        ResponseResult::Apps { apps } => apps
            .iter()
            .map(|app| format!("{}\t{}", app.id, app.name))
            .collect::<Vec<_>>()
            .join("\n"),
        ResponseResult::Sessions { sessions } => sessions
            .iter()
            .map(|session| {
                format!(
                    "{}\t{}\t{:?}\t{}",
                    session.name, session.app_id, session.status, session.client_count
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        ResponseResult::Session { session } => session.name.clone(),
        ResponseResult::Attach { attach } => attach.socket_path.clone(),
        ResponseResult::Ack => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use navette_protocol::{ApiError, ErrorCode, ResponseOutcome, Session, SessionStatus};

    use super::*;

    #[test]
    fn renders_sessions_deterministically() {
        let result = ResponseResult::Sessions {
            sessions: vec![Session {
                name: "work".into(),
                app_id: "firefox.desktop".into(),
                app_pid: 10,
                daemon_pid: 11,
                wayland_display: "navette-work".into(),
                socket_path: "/run/navette/work/wprs.sock".into(),
                created_at_ms: 1,
                last_attached_at_ms: None,
                client_count: 2,
                status: SessionStatus::Running,
            }],
        };
        assert_eq!(render_result(&result), "work\tfirefox.desktop\tRunning\t2");
    }

    #[test]
    fn surfaces_structured_daemon_errors() {
        let error = response_result(Response {
            request_id: 1,
            outcome: ResponseOutcome::Error {
                error: ApiError {
                    code: ErrorCode::NotFound,
                    message: "session not found: work".into(),
                },
            },
        })
        .unwrap_err();
        assert!(error.to_string().contains("NotFound"));
        assert!(error.to_string().contains("session not found: work"));
    }
}
