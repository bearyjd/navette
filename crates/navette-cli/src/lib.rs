use std::path::Path;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use navette_auth::SecretString;
use navette_protocol::{
    Request, RequestCommand, Response, ResponseOutcome, ResponseResult, WEBSOCKET_SUBPROTOCOL,
};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::io::ReaderStream;
use url::Url;

#[derive(Clone, Debug)]
pub struct Client {
    url: String,
    token: SecretString,
}

/// Metadata reserved before a file's bytes are uploaded.
#[derive(Clone, Debug, Deserialize)]
pub struct FilePreflightResponse {
    pub transfer_id: String,
    pub upload_url: String,
    pub expires_at: u64,
}

/// The daemon's view of a file transfer.
#[derive(Clone, Debug, Deserialize)]
pub struct FileTransferStatus {
    pub transfer_id: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub bytes_received: u64,
    pub state: FileTransferState,
    pub expires_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileTransferState {
    AwaitingUpload,
    Queued,
    Materializing,
    Delivered,
    Failed,
    Cancelled,
}

#[derive(Serialize)]
struct FilePreflight<'a> {
    name: &'a str,
    mime: &'a str,
    size: u64,
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

    /// Reserves a server-owned destination before sending any file content.
    pub async fn preflight_file(
        &self,
        session: &str,
        name: &str,
        mime: &str,
        size: u64,
    ) -> Result<FilePreflightResponse> {
        let url = self.file_collection_url(session)?;
        let response = self
            .http_client()?
            .post(url)
            .header(reqwest::header::AUTHORIZATION, self.authorization_header()?)
            .json(&FilePreflight { name, mime, size })
            .send()
            .await
            .context("failed to preflight file transfer")?;
        json_response(response, "file preflight").await
    }

    /// Streams a file to the relative URL returned from [`Self::preflight_file`].
    /// The daemon URL is only used as an origin; a returned path can never
    /// replace its host, scheme, or authority.
    pub async fn upload_file(
        &self,
        upload_url: &str,
        source: &Path,
        size: u64,
    ) -> Result<FileTransferStatus> {
        let url = self.relative_http_url(upload_url)?;
        let file = tokio::fs::File::open(source)
            .await
            .with_context(|| format!("failed to open {} for upload", source.display()))?;
        let response = self
            .http_client()?
            .put(url)
            .header(reqwest::header::AUTHORIZATION, self.authorization_header()?)
            .header(reqwest::header::CONTENT_LENGTH, size)
            .body(reqwest::Body::wrap_stream(ReaderStream::new(file)))
            .send()
            .await
            .context("failed to upload file")?;
        json_response(response, "file upload").await
    }

    pub async fn file_status(
        &self,
        session: &str,
        transfer_id: &str,
    ) -> Result<FileTransferStatus> {
        let url = self.file_url(session, transfer_id)?;
        let response = self
            .http_client()?
            .get(url)
            .header(reqwest::header::AUTHORIZATION, self.authorization_header()?)
            .send()
            .await
            .context("failed to fetch file transfer status")?;
        json_response(response, "file transfer status").await
    }

    /// Cancellation is intentionally idempotent from the CLI's point of view:
    /// an upload error must retain its useful original cause even if cleanup
    /// races materialization or the daemon is unavailable.
    pub async fn cancel_file(&self, session: &str, transfer_id: &str) -> Result<()> {
        let url = self.file_url(session, transfer_id)?;
        let response = self
            .http_client()?
            .delete(url)
            .header(reqwest::header::AUTHORIZATION, self.authorization_header()?)
            .send()
            .await
            .context("failed to cancel file transfer")?;
        if response.status() == StatusCode::NO_CONTENT {
            Ok(())
        } else {
            response_error(response, "file transfer cancellation").await
        }
    }

    fn authorization_header(&self) -> Result<reqwest::header::HeaderValue> {
        format!("Bearer {}", self.token.as_str())
            .parse()
            .context("token is not a valid HTTP header value")
    }

    fn http_client(&self) -> Result<reqwest::Client> {
        // Do not let a redirect turn an authenticated daemon request into a
        // request to another authority. The API never needs redirects.
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to configure HTTP client")
    }

    fn file_collection_url(&self, session: &str) -> Result<Url> {
        self.endpoint_url(["v1", "sessions", session, "files"])
    }

    fn file_url(&self, session: &str, transfer_id: &str) -> Result<Url> {
        self.endpoint_url(["v1", "sessions", session, "files", transfer_id])
    }

    fn endpoint_url<const N: usize>(&self, segments: [&str; N]) -> Result<Url> {
        let mut url = self.http_base_url()?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("daemon URL cannot be a base URL"))?
            .clear()
            .extend(segments);
        Ok(url)
    }

    fn relative_http_url(&self, relative: &str) -> Result<Url> {
        // `Url::join` deliberately treats `//host/path` as a new authority,
        // and special URLs treat backslashes as path separators. File API
        // URLs are server-provided paths, never URLs, so reject both forms
        // before joining rather than forwarding our bearer token elsewhere.
        if !relative.starts_with('/') || relative.starts_with("//") || relative.contains('\\') {
            bail!("daemon returned an invalid file upload path")
        }
        let base = self.http_base_url()?;
        let url = base
            .join(relative)
            .context("daemon returned an invalid file upload path")?;
        if url.origin() != base.origin() {
            bail!("daemon returned an invalid file upload path")
        }
        Ok(url)
    }

    fn http_base_url(&self) -> Result<Url> {
        let mut url =
            Url::parse(&self.url).with_context(|| format!("invalid daemon URL: {}", self.url))?;
        match url.scheme() {
            "ws" => url
                .set_scheme("http")
                .expect("http is a valid replacement for ws"),
            "wss" => url
                .set_scheme("https")
                .expect("https is a valid replacement for wss"),
            scheme => bail!("daemon URL must use ws:// or wss://, not {scheme}://"),
        }
        url.set_query(None);
        url.set_fragment(None);
        Ok(url)
    }
}

async fn json_response<T: DeserializeOwned>(
    response: reqwest::Response,
    operation: &str,
) -> Result<T> {
    let response = success_response(response, operation).await?;
    response
        .json()
        .await
        .with_context(|| format!("daemon returned an invalid {operation} response"))
}

async fn success_response(
    response: reqwest::Response,
    operation: &str,
) -> Result<reqwest::Response> {
    if response.status().is_success() {
        Ok(response)
    } else {
        response_error(response, operation).await?;
        unreachable!("response_error always returns an error")
    }
}

async fn response_error(response: reqwest::Response, operation: &str) -> Result<()> {
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED {
        bail!(
            "{operation} was rejected (HTTP 401). The daemon's token has probably been rotated: run `navette token` on the daemon host for the current value, then pass it as --token or NAVETTE_TOKEN"
        );
    }
    let body = response.text().await.unwrap_or_default();
    let detail = body.trim();
    if detail.is_empty() {
        bail!("{operation} failed with HTTP {status}");
    }
    bail!("{operation} failed with HTTP {status}: {detail}")
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

    #[test]
    fn file_endpoints_reuse_the_daemon_authority_but_not_its_websocket_path() {
        let client = Client::new(
            "ws://[::1]:19417/v1/ws?ignored=yes#fragment",
            "token".to_owned(),
        );
        assert_eq!(
            client.file_collection_url("my session").unwrap().as_str(),
            "http://[::1]:19417/v1/sessions/my%20session/files"
        );
        assert_eq!(
            client
                .relative_http_url("/v1/sessions/work/files/deadbeef/content")
                .unwrap()
                .as_str(),
            "http://[::1]:19417/v1/sessions/work/files/deadbeef/content"
        );
    }

    #[test]
    fn file_upload_urls_cannot_replace_the_daemon_authority() {
        let client = Client::new("wss://tower.example:9417/v1/ws", "token".to_owned());
        assert_eq!(
            client.http_base_url().unwrap().as_str(),
            "https://tower.example:9417/v1/ws"
        );
        let error = client
            .relative_http_url("//evil.example/upload")
            .unwrap_err();
        assert!(error.to_string().contains("invalid file upload path"));
        let error = client
            .relative_http_url("https://evil.example/upload")
            .unwrap_err();
        assert!(error.to_string().contains("invalid file upload path"));
        let error = client
            .relative_http_url("/\\\\evil.example/upload")
            .unwrap_err();
        assert!(error.to_string().contains("invalid file upload path"));
    }
}
