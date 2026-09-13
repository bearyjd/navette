use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use navette_auth::SecretString;
use navette_protocol::{
    Request, RequestCommand, Response, ResponseOutcome, ResponseResult, WEBSOCKET_SUBPROTOCOL,
};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[derive(Clone, Debug)]
pub struct Client {
    url: String,
    token: SecretString,
}

impl Client {
    pub fn new(url: impl Into<String>, token: impl Into<SecretString>) -> Self {
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
            format!("Bearer {}", self.token.as_str())
                .parse()
                .context("token is not a valid header value")?,
        );
        let (mut socket, response) = connect_async(upgrade)
            .await
            .map_err(|error| connect_error(error, &self.url))?;
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

/// Turns a failed WebSocket upgrade into something an operator can act on.
///
/// Every route requires a bearer token, so the most likely reason a working
/// invocation stops working is a rotated one — and tungstenite surfaces that as
/// `HTTP error: 401`, which names neither the cause nor the remedy. Kept a free
/// function so the mapping can be tested against a synthetic response rather
/// than a live daemon.
fn connect_error(error: tokio_tungstenite::tungstenite::Error, url: &str) -> anyhow::Error {
    use tokio_tungstenite::tungstenite::Error;
    use tokio_tungstenite::tungstenite::http::StatusCode;

    if let Error::Http(response) = &error
        && response.status() == StatusCode::UNAUTHORIZED
    {
        return anyhow::anyhow!(
            "{url} rejected the token (HTTP 401). The daemon's token has probably been rotated: run `navette token` on the daemon host for the current value, then pass it as --token or NAVETTE_TOKEN"
        );
    }
    anyhow::Error::new(error).context(format!("failed to connect to {url}"))
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

    #[test]
    fn a_401_upgrade_names_the_command_that_shows_the_token() {
        // Design §7: a rotated token must not present as a bare
        // `HTTP error: 401`, which reads like the daemon is down.
        use tokio_tungstenite::tungstenite::http::{Response as HttpResponse, StatusCode};

        let refusal = HttpResponse::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(None)
            .unwrap();
        let error = connect_error(
            tokio_tungstenite::tungstenite::Error::Http(Box::new(refusal)),
            "ws://127.0.0.1:9417/v1/ws",
        );
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("navette token"),
            "a 401 must name the remedy: {rendered}"
        );
        assert!(rendered.contains("401"), "and the status: {rendered}");
    }

    #[test]
    fn a_non_401_failure_keeps_the_connection_context() {
        // The 401 branch must not swallow every other failure into an auth
        // message: a daemon that isn't running still has to say so.
        let error = connect_error(
            tokio_tungstenite::tungstenite::Error::ConnectionClosed,
            "ws://127.0.0.1:9417/v1/ws",
        );
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("failed to connect to ws://127.0.0.1:9417/v1/ws"),
            "{rendered}"
        );
        assert!(!rendered.contains("navette token"), "{rendered}");
    }
}
