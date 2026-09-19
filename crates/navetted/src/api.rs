use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response as HttpResponse};
use axum::routing::{any, get, post, put};
use futures_util::{SinkExt, StreamExt};
use navette_protocol::media::{
    MAX_INPUT_MESSAGE, MEDIA_WEBSOCKET_SUBPROTOCOL, MediaInput, MediaServerMessage,
};
use navette_protocol::{
    API_VERSION, AttachInfo, ErrorCode, Request, RequestCommand, Response, ResponseResult,
    WEBSOCKET_SUBPROTOCOL,
};
use navette_wake::{
    DEFAULT_BROADCAST, DEFAULT_PORT, LAN_BROADCAST_RULE, MacAddress, is_lan_broadcast_target,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex as AsyncMutex, Notify, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};
use tokio::time::{Duration, Instant, sleep_until, timeout};

use crate::app_index::AppIndex;
use crate::blobs::{BlobStore, BlobStoreError};
use crate::bridge::BridgeManager;
use crate::file_transfers::{
    FilePreflight, FileSessionLease, FileTransferError, FileTransferStore,
};
use crate::media::{MediaHub, MediaHubError};
use crate::registry::{RegistryError, default_session_name, validate_session_name};
use crate::supervisor::{ProcessRunner, Supervisor, SupervisorError};
use crate::thumbnails::ThumbnailStore;

const MAX_CONTROL_MESSAGE_SIZE: usize = 1024 * 1024;
const MAX_INPUT_MESSAGES_PER_SECOND: u32 = 240;
const MAX_CONCURRENT_BLOB_UPLOADS: usize = 4;
const MAX_CONCURRENT_BLOB_DOWNLOADS: usize = 4;
/// Two is plenty for a person waking a machine and a ceiling on what an
/// authenticated client can make the daemon broadcast.
const MAX_CONCURRENT_WAKES: usize = 2;
const BLOB_UPLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const BLOB_UPLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
const BLOB_READ_CHUNK_BYTES: usize = 64 * 1024;
const BLOB_DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const BLOB_DOWNLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
/// A 256x256 PNG app icon is tens of kilobytes; anything past this is not an
/// icon and is not read into memory to find out.
const MAX_ICON_BYTES: u64 = 1024 * 1024;
/// A drawer of fifty apps asks for fifty icons at once; four at a time keeps
/// that off the disk without anyone noticing, and callers wait rather than
/// get a 429 that would show up as a missing tile.
const MAX_CONCURRENT_ICON_READS: usize = 4;
/// The eight bytes every PNG starts with (PNG spec §5.2).
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
/// Thumbnails change while a session runs, so clients revalidate every time
/// and get a 304 when nothing has.
const THUMBNAIL_CACHE_CONTROL: &str = "no-cache";
/// Icons change when packages do. An hour is short enough that an upgrade
/// shows up and long enough that a drawer refresh is free.
const ICON_CACHE_CONTROL: &str = "max-age=3600";

/// Holds a transfer slot only while a response is making progress. The
/// watchdog releases a stalled reader's permit even when Hyper stops polling
/// the body stream because the peer stopped consuming it.
struct DownloadLease {
    permit: Mutex<Option<OwnedSemaphorePermit>>,
    last_progress: Mutex<Instant>,
    expired: AtomicBool,
    finished: AtomicBool,
    wake: Notify,
}

impl DownloadLease {
    fn new(permit: OwnedSemaphorePermit) -> Arc<Self> {
        Arc::new(Self {
            permit: Mutex::new(Some(permit)),
            last_progress: Mutex::new(Instant::now()),
            expired: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            wake: Notify::new(),
        })
    }

    fn touch(&self) {
        if let Ok(mut last_progress) = self.last_progress.lock() {
            *last_progress = Instant::now();
        }
        self.wake.notify_one();
    }

    fn finish(&self) {
        self.finished.store(true, Ordering::Release);
        if let Ok(mut permit) = self.permit.lock() {
            permit.take();
        }
        self.wake.notify_waiters();
    }

    async fn watch(self: Arc<Self>) {
        let deadline = Instant::now() + BLOB_DOWNLOAD_TOTAL_TIMEOUT;
        loop {
            if self.finished.load(Ordering::Acquire) {
                return;
            }
            let idle_deadline = self
                .last_progress
                .lock()
                .map(|last_progress| *last_progress + BLOB_DOWNLOAD_IDLE_TIMEOUT)
                .unwrap_or(deadline);
            let wake_at = idle_deadline.min(deadline);
            tokio::select! {
                _ = sleep_until(wake_at) => {
                    let now = Instant::now();
                    let idle = self.last_progress.lock().is_ok_and(|last_progress| {
                        now.saturating_duration_since(*last_progress) >= BLOB_DOWNLOAD_IDLE_TIMEOUT
                    });
                    if now >= deadline || idle {
                        self.expired.store(true, Ordering::Release);
                        self.finish();
                        return;
                    }
                }
                _ = self.wake.notified() => {}
            }
        }
    }
}

pub struct ApiState<R: ProcessRunner> {
    pub apps: Arc<AppIndex>,
    pub supervisor: Arc<Supervisor<R>>,
    pub media: MediaHub,
    pub blobs: BlobStore,
    pub files: FileTransferStore,
    pub thumbnails: ThumbnailStore,
    pub bridges: BridgeManager,
    pub auth: Arc<navette_auth::AuthToken>,
    upload_slots: Arc<Semaphore>,
    download_slots: Arc<Semaphore>,
    icon_slots: Arc<Semaphore>,
    wake_slots: Arc<Semaphore>,
    /// Where a wake request that names no `broadcast` is sent. The phone never
    /// names one, so on a multi-homed relay this is what makes it reach the LAN.
    wake_broadcast: Ipv4Addr,
    lifecycle_locks: Arc<AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
}

impl<R: ProcessRunner> Clone for ApiState<R> {
    fn clone(&self) -> Self {
        Self {
            apps: Arc::clone(&self.apps),
            supervisor: Arc::clone(&self.supervisor),
            media: self.media.clone(),
            blobs: self.blobs.clone(),
            files: self.files.clone(),
            thumbnails: self.thumbnails.clone(),
            bridges: self.bridges.clone(),
            auth: Arc::clone(&self.auth),
            upload_slots: Arc::clone(&self.upload_slots),
            download_slots: Arc::clone(&self.download_slots),
            icon_slots: Arc::clone(&self.icon_slots),
            wake_slots: Arc::clone(&self.wake_slots),
            wake_broadcast: self.wake_broadcast,
            lifecycle_locks: Arc::clone(&self.lifecycle_locks),
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
        let files = FileTransferStore::new(supervisor.drop_root());
        let thumbnails = ThumbnailStore::default();
        if let Ok(registry) = supervisor.registry().lock() {
            for session in registry.list() {
                if session.status == navette_protocol::SessionStatus::Running {
                    let _ = blobs.activate(&session.name);
                    if let Err(error) = files.recover(&session.name) {
                        tracing::warn!(
                            session = %session.name,
                            %error,
                            "file transfer recovery failed; transfers disabled for this session until restart"
                        );
                    }
                }
            }
        }
        Self {
            apps,
            supervisor,
            bridges: BridgeManager::with_transfers(
                media.clone(),
                blobs.clone(),
                files.clone(),
                thumbnails.clone(),
            ),
            media,
            blobs,
            files,
            thumbnails,
            auth,
            upload_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_BLOB_UPLOADS)),
            download_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_BLOB_DOWNLOADS)),
            icon_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_ICON_READS)),
            wake_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_WAKES)),
            wake_broadcast: DEFAULT_BROADCAST,
            lifecycle_locks: Arc::new(AsyncMutex::new(HashMap::new())),
        }
    }

    /// Sets the broadcast address used when a wake request omits one. The
    /// caller (`--wake-broadcast`) is responsible for having checked it
    /// against [`navette_wake::is_lan_broadcast_target`].
    pub fn with_wake_broadcast(self, wake_broadcast: Ipv4Addr) -> Self {
        Self {
            wake_broadcast,
            ..self
        }
    }

    /// Serializes destructive lifecycle transitions for the same session
    /// name. In particular, a successful old Kill cannot deactivate blobs or
    /// stop a bridge after a concurrent Run has recreated that name.
    async fn lock_session_lifecycle(&self, session: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.lifecycle_locks.lock().await;
            Arc::clone(
                locks
                    .entry(session.to_owned())
                    .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
            )
        };
        lock.lock_owned().await
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
        .route(
            "/v1/sessions/{session}/thumbnail",
            get(session_thumbnail::<R>),
        )
        .route("/v1/apps/{id}/icon", get(app_icon::<R>))
        .route("/v1/sessions/{session}/files", post(create_file::<R>))
        .route(
            "/v1/sessions/{session}/files/{transfer_id}",
            get(file_status::<R>).delete(cancel_file::<R>),
        )
        .route(
            "/v1/sessions/{session}/files/{transfer_id}/content",
            put(upload_file::<R>),
        )
        .route("/v1/wake", post(wake_host::<R>))
        .layer(axum::middleware::from_fn_with_state(
            auth,
            crate::guard::authenticate,
        ))
        .layer(axum::middleware::from_fn(
            crate::guard::reject_browser_origin,
        ))
        .with_state(state)
}

/// Body of `POST /v1/wake`. `broadcast` is an IPv4 literal and `port` is
/// 1-65535; both default when omitted.
#[derive(Debug, Deserialize)]
pub struct WakeRequest {
    pub mac: String,
    pub broadcast: Option<String>,
    pub port: Option<u16>,
}

/// A validated wake request, ready to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WakeTarget {
    mac: MacAddress,
    broadcast: Ipv4Addr,
    port: u16,
}

/// Every failure here is the client's and becomes a 400 carrying the reason:
/// a body that is not JSON, a MAC or broadcast address that does not parse,
/// port 0, or a broadcast address outside the ranges a LAN relay may send to.
fn parse_wake_request(
    body: &[u8],
    default_broadcast: Ipv4Addr,
) -> Result<WakeTarget, &'static str> {
    let request: WakeRequest =
        serde_json::from_slice(body).map_err(|_| "body is not a valid wake request")?;
    let mac = request.mac.parse().map_err(|_| "invalid MAC address")?;
    let broadcast = match request.broadcast {
        Some(literal) => literal
            .parse()
            .map_err(|_| "broadcast must be an IPv4 literal")?,
        None => default_broadcast,
    };
    if !is_lan_broadcast_target(broadcast) {
        return Err(LAN_BROADCAST_RULE);
    }
    let port = request.port.unwrap_or(DEFAULT_PORT);
    if port == 0 {
        return Err("port must be 1-65535");
    }
    Ok(WakeTarget {
        mac,
        broadcast,
        port,
    })
}

/// Relays a wake-on-LAN magic packet onto the daemon's LAN. The phone lives
/// on the tailnet and cannot broadcast onto the home network itself.
///
/// The body is read as raw bytes rather than through `Json<_>`: axum's
/// extractor answers a missing Content-Type with 415 and a wrong-shaped body
/// with 422, and the contract the clients code against is 400 for anything
/// short of a valid request.
async fn wake_host<R: ProcessRunner>(
    State(state): State<ApiState<R>>,
    body: Bytes,
) -> HttpResponse {
    let target = match parse_wake_request(&body, state.wake_broadcast) {
        Ok(target) => target,
        Err(reason) => return (StatusCode::BAD_REQUEST, reason).into_response(),
    };
    // Held across the blocking send, like the upload slots: the relay is a
    // small fixed budget, not a queue.
    let _wake_permit = match Arc::clone(&state.wake_slots).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return StatusCode::TOO_MANY_REQUESTS.into_response(),
    };
    let WakeTarget {
        mac,
        broadcast,
        port,
    } = target;
    let sent =
        tokio::task::spawn_blocking(move || navette_wake::send_magic_packet(mac, broadcast, port))
            .await;
    match sent {
        Ok(Ok(())) => {
            tracing::info!(%mac, %broadcast, port, "sent wake-on-LAN magic packet");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Err(error)) => {
            tracing::warn!(%mac, %broadcast, port, %error, "wake-on-LAN send failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(error) => {
            tracing::error!(%mac, %error, "wake-on-LAN send task did not complete");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn create_file<R: ProcessRunner>(
    Path(session): Path<String>,
    State(state): State<ApiState<R>>,
    Json(request): Json<FilePreflight>,
) -> HttpResponse {
    let lease = match live_file_lease(&state, &session) {
        Ok(lease) => lease,
        Err(error) => return file_error_response(error),
    };
    match state.files.preflight_with_lease(&lease, request) {
        Ok(response) => (StatusCode::CREATED, Json(response)).into_response(),
        Err(error) => file_error_response(error),
    }
}

/// Samples the file-store incarnation before checking the supervisor record.
/// If Kill+Run reuses the name between this point and `preflight_with_lease`,
/// the epoch check rejects the stale request instead of attaching it to the
/// replacement session.
fn live_file_lease<R: ProcessRunner>(
    state: &ApiState<R>,
    session: &str,
) -> Result<FileSessionLease, FileTransferError> {
    let lease = state.files.lease(session)?;
    is_live_session(state, session)
        .then_some(lease)
        .ok_or(FileTransferError::SessionNotLive)
}

async fn upload_file<R: ProcessRunner>(
    Path((session, transfer_id)): Path<(String, String)>,
    State(state): State<ApiState<R>>,
    headers: HeaderMap,
    body: Body,
) -> HttpResponse {
    if !is_live_session(&state, &session) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let expected = match state.files.status(&session, &transfer_id) {
        Ok(status) => status.size,
        Err(error) => return file_error_response(error),
    };
    let declared = match headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        Some(length) if length == expected => length,
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    debug_assert_eq!(declared, expected);
    let _upload_permit = match Arc::clone(&state.upload_slots).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return StatusCode::TOO_MANY_REQUESTS.into_response(),
    };
    let mut upload = match state.files.begin_upload(&session, &transfer_id) {
        Ok(upload) => upload,
        Err(error) => return file_error_response(error),
    };
    let mut stream = body.into_data_stream();
    let write_body = async {
        while let Some(chunk) = timeout(BLOB_UPLOAD_IDLE_TIMEOUT, stream.next())
            .await
            .map_err(|_| StatusCode::REQUEST_TIMEOUT.into_response())?
        {
            let chunk = chunk.map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
            upload.write_chunk(&chunk).map_err(file_error_response)?;
        }
        upload.finish().map_err(file_error_response)
    };
    match timeout(BLOB_UPLOAD_TOTAL_TIMEOUT, write_body).await {
        Ok(Ok(status)) => {
            state.bridges.materialize_file(&session, &transfer_id);
            (StatusCode::ACCEPTED, Json(status)).into_response()
        }
        Ok(Err(response)) => response,
        Err(_) => StatusCode::REQUEST_TIMEOUT.into_response(),
    }
}

async fn file_status<R: ProcessRunner>(
    Path((session, transfer_id)): Path<(String, String)>,
    State(state): State<ApiState<R>>,
) -> HttpResponse {
    if !is_live_session(&state, &session) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match state.files.status(&session, &transfer_id) {
        Ok(status) => Json(status).into_response(),
        Err(error) => file_error_response(error),
    }
}

async fn cancel_file<R: ProcessRunner>(
    Path((session, transfer_id)): Path<(String, String)>,
    State(state): State<ApiState<R>>,
) -> HttpResponse {
    if !is_live_session(&state, &session) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match state.files.cancel(&session, &transfer_id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => file_error_response(error),
    }
}

async fn upload_blob<R: ProcessRunner>(
    Path(session): Path<String>,
    State(state): State<ApiState<R>>,
    headers: HeaderMap,
    body: Body,
) -> HttpResponse {
    if !is_live_session(&state, &session) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(mime) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    };
    if mime.contains(';') {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let _upload_permit = match Arc::clone(&state.upload_slots).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return StatusCode::TOO_MANY_REQUESTS.into_response(),
    };

    let mut writer = match state.blobs.begin_write(&session, mime) {
        Ok(writer) => writer,
        Err(error) => return blob_error_response(error),
    };
    let mut stream = body.into_data_stream();
    let write_body = async {
        while let Some(chunk) = timeout(BLOB_UPLOAD_IDLE_TIMEOUT, stream.next())
            .await
            .map_err(|_| StatusCode::REQUEST_TIMEOUT.into_response())?
        {
            let chunk = chunk.map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
            writer.write_chunk(&chunk).map_err(blob_error_response)?;
        }
        writer
            .finish()
            .map(|blob| (StatusCode::CREATED, Json(blob)).into_response())
            .map_err(blob_error_response)
    };
    match timeout(BLOB_UPLOAD_TOTAL_TIMEOUT, write_body).await {
        Ok(Ok(response)) => response,
        Ok(Err(response)) => response,
        Err(_) => StatusCode::REQUEST_TIMEOUT.into_response(),
    }
}

async fn download_blob<R: ProcessRunner>(
    Path((session, id)): Path<(String, String)>,
    State(state): State<ApiState<R>>,
) -> HttpResponse {
    if !is_live_session(&state, &session) {
        return StatusCode::NOT_FOUND.into_response();
    }
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
    let download_permit = match Arc::clone(&state.download_slots).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return StatusCode::TOO_MANY_REQUESTS.into_response(),
    };
    let path = match state.blobs.read_path(&session, &blob) {
        Ok(path) => path,
        Err(error) => return blob_error_response(error),
    };
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) => return blob_error_response(BlobStoreError::Io(error)),
    };
    let lease = DownloadLease::new(download_permit);
    tokio::spawn(Arc::clone(&lease).watch());
    let stream = futures_util::stream::try_unfold((file, lease), |(mut file, lease)| async move {
        if lease.expired.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "blob download timed out",
            ));
        }
        let mut buffer = vec![0; BLOB_READ_CHUNK_BYTES];
        let read = match file.read(&mut buffer).await {
            Ok(read) => read,
            Err(error) => {
                lease.finish();
                return Err(error);
            }
        };
        if read == 0 {
            lease.finish();
            Ok::<_, std::io::Error>(None)
        } else {
            buffer.truncate(read);
            lease.touch();
            Ok(Some((Bytes::from(buffer), (file, lease))))
        }
    });
    let mut response = Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        blob.mime.parse().expect("BlobDescriptor validates MIME"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        blob.size
            .to_string()
            .parse()
            .expect("u64 content length is always a valid header"),
    );
    response
}

/// The session's most recent snapshot as `image/jpeg`. 404 until the encode
/// thread has taken one (a session that has never painted has nothing to
/// show) and for any session that is not live, like every session route.
async fn session_thumbnail<R: ProcessRunner>(
    Path(session): Path<String>,
    State(state): State<ApiState<R>>,
    headers: HeaderMap,
) -> HttpResponse {
    if !is_live_session(&state, &session) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(thumbnail) = state.thumbnails.get(&session) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if if_none_match_matches(&headers, &thumbnail.etag) {
        return not_modified(&thumbnail.etag, THUMBNAIL_CACHE_CONTROL);
    }
    image_response(
        "image/jpeg",
        &thumbnail.etag,
        THUMBNAIL_CACHE_CONTROL,
        thumbnail.jpeg.clone(),
    )
}

/// The app's PNG icon, resolved from the freedesktop icon dirs when the index
/// was loaded. 404 for an unknown app and for one whose `Icon=` has no PNG
/// (SVG-only themes included): the drawer falls back to a placeholder, so a
/// missing icon is not an error worth distinguishing.
///
/// The path was resolved at index load from the host's own data dirs, never
/// from the request, so nothing here joins client input into a filesystem
/// path. The file is re-checked on every request rather than cached: an
/// icon theme upgrade should show up, and a stat is cheap. What is served
/// has to *be* a PNG -- signature checked, size bounded on the read itself
/// and not only on the stat before it -- because the client will hand the
/// body straight to an image decoder.
async fn app_icon<R: ProcessRunner>(
    Path(id): Path<String>,
    State(state): State<ApiState<R>>,
    headers: HeaderMap,
) -> HttpResponse {
    let Some(path) = state.apps.icon_path(&id).map(std::path::Path::to_path_buf) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Waits for a slot rather than answering 429: a drawer refresh fans out
    // one request per app, and a tile that failed to load is what the user
    // would see.
    let Ok(_slot) = Arc::clone(&state.icon_slots).acquire_owned().await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) => {
            tracing::debug!(app = %id, path = %path.display(), %error, "app icon unreadable");
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    let metadata = match file.metadata().await {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            tracing::debug!(app = %id, path = %path.display(), %error, "app icon unreadable");
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    if metadata.len() > MAX_ICON_BYTES {
        tracing::debug!(app = %id, path = %path.display(), size = metadata.len(), "app icon too large to serve");
        return StatusCode::NOT_FOUND.into_response();
    }
    let etag = icon_etag(&metadata);
    if if_none_match_matches(&headers, &etag) {
        return not_modified(&etag, ICON_CACHE_CONTROL);
    }
    match read_png_bounded(file).await {
        Ok(bytes) => image_response("image/png", &etag, ICON_CACHE_CONTROL, bytes),
        Err(reason) => {
            tracing::debug!(app = %id, path = %path.display(), reason, "app icon not served");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// Reads at most `MAX_ICON_BYTES` from an open file and checks it starts
/// like a PNG. The bound is on the read, not on a stat taken earlier, so a
/// file that grows between the two cannot get past it.
async fn read_png_bounded(file: tokio::fs::File) -> Result<Bytes, &'static str> {
    let mut bytes = Vec::new();
    file.take(MAX_ICON_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "read failed")?;
    if bytes.len() as u64 > MAX_ICON_BYTES {
        return Err("larger than the icon limit");
    }
    if !bytes.starts_with(&PNG_SIGNATURE) {
        return Err("not a PNG");
    }
    Ok(Bytes::from(bytes))
}

/// `"<mtime ms hex>-<len hex>"`, the same shape as a thumbnail's etag. Both
/// inputs change when the package manager replaces the file.
fn icon_etag(metadata: &std::fs::Metadata) -> String {
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    format!("\"{modified_ms:x}-{:x}\"", metadata.len())
}

/// Whether any entity tag the client presented matches `etag`. The header is
/// a comma-separated list; `W/` weak prefixes are ignored on the client side
/// because `If-None-Match` compares weakly (RFC 9110 §13.1.2), and `*`
/// matches anything that exists.
fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| {
            candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
        })
}

fn validator_headers(etag: &str, cache_control: &'static str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(etag) = HeaderValue::from_str(etag) {
        headers.insert(header::ETAG, etag);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    headers
}

/// 304 with the validators and no body. axum's router stamps
/// `Content-Length: 0` on the in-process response, but hyper writes no
/// `Content-Length` at all for a 304 (it drops an explicit one too), so the
/// wire is clean without anything done here.
fn not_modified(etag: &str, cache_control: &'static str) -> HttpResponse {
    (
        StatusCode::NOT_MODIFIED,
        validator_headers(etag, cache_control),
    )
        .into_response()
}

fn image_response(
    content_type: &'static str,
    etag: &str,
    cache_control: &'static str,
    body: Bytes,
) -> HttpResponse {
    let mut headers = validator_headers(etag, cache_control);
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CONTENT_LENGTH,
        body.len()
            .to_string()
            .parse()
            .expect("a usize content length is always a valid header"),
    );
    (StatusCode::OK, headers, body).into_response()
}

fn is_live_session<R: ProcessRunner>(state: &ApiState<R>, session: &str) -> bool {
    validate_session_name(session).is_ok()
        && state.supervisor.registry().lock().is_ok_and(|registry| {
            registry
                .get(session)
                .is_some_and(|record| record.status == navette_protocol::SessionStatus::Running)
        })
}

fn blob_error_response(error: BlobStoreError) -> HttpResponse {
    match error {
        BlobStoreError::UnsupportedMime => StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response(),
        BlobStoreError::BlobTooLarge => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        BlobStoreError::SessionBudgetExceeded => StatusCode::INSUFFICIENT_STORAGE.into_response(),
        BlobStoreError::InvalidSession
        | BlobStoreError::InvalidDescriptor
        | BlobStoreError::NotFound
        | BlobStoreError::SessionNotLive => StatusCode::NOT_FOUND.into_response(),
        BlobStoreError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn file_error_response(error: FileTransferError) -> HttpResponse {
    match error {
        FileTransferError::InvalidSession
        | FileTransferError::NotFound
        | FileTransferError::SessionNotLive => StatusCode::NOT_FOUND.into_response(),
        FileTransferError::InvalidName
        | FileTransferError::InvalidMime
        | FileTransferError::SizeMismatch => StatusCode::BAD_REQUEST.into_response(),
        FileTransferError::FileTooLarge => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        FileTransferError::SessionBudgetExceeded => {
            StatusCode::INSUFFICIENT_STORAGE.into_response()
        }
        FileTransferError::UploadInProgress
        | FileTransferError::InvalidState
        | FileTransferError::NotCancellable
        | FileTransferError::Cancelled => StatusCode::CONFLICT.into_response(),
        FileTransferError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
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
            let session_name = name.unwrap_or_else(|| default_session_name(&app.id));
            if let Err(error) = validate_session_name(&session_name) {
                return Response::error(
                    request_id,
                    ApiFailure::from(error).code,
                    "invalid session name",
                );
            }
            let _lifecycle = state.lock_session_lifecycle(&session_name).await;
            let result = state.supervisor.start(app, Some(&session_name)).await;
            match result {
                Ok(session) => {
                    // The registry has accepted the name, so anything the
                    // store still holds under it is a previous session's --
                    // one that exited on its own and was never killed, since
                    // Kill clears its own entry -- and must not become this
                    // session's first picture. Done here, not before
                    // `start`, so a Run rejected for an existing name does
                    // not wipe that live session's thumbnail; and before the
                    // bridge starts, so it cannot race a fresh snapshot.
                    state.thumbnails.remove(&session.name);
                    match state.blobs.activate(&session.name) {
                        Err(error) => {
                            let _ = state.supervisor.kill(&session.name).await;
                            Err(ApiFailure::internal(format!(
                                "failed to initialize clipboard blob namespace: {error}"
                            )))
                        }
                        Ok(()) => match state.files.activate_prepared(&session.name) {
                            Err(error) => {
                                let _ = state.blobs.deactivate(&session.name);
                                let _ = state.supervisor.kill(&session.name).await;
                                Err(ApiFailure::internal(format!(
                                    "failed to initialize file drop namespace: {error}"
                                )))
                            }
                            Ok(()) => match state.bridges.start(&session) {
                                Ok(()) => Ok(ResponseResult::Session { session }),
                                Err(error) => {
                                    let _ = state.files.deactivate(&session.name);
                                    let _ = state.blobs.deactivate(&session.name);
                                    let _ = state.supervisor.kill(&session.name).await;
                                    Err(ApiFailure::internal(format!(
                                        "failed to start media bridge: {error}"
                                    )))
                                }
                            },
                        },
                    }
                }
                Err(error) => Err(ApiFailure::from(error)),
            }
        }
        RequestCommand::Kill { session } => {
            if let Err(error) = validate_session_name(&session) {
                Err(ApiFailure::from(error))
            } else {
                let _lifecycle = state.lock_session_lifecycle(&session).await;
                state
                    .supervisor
                    .kill(&session)
                    .await
                    .map(|_| {
                        // Do not revoke a running session's blob writers or
                        // unregister its bridge until process teardown and
                        // registry removal succeeded. A failed kill must leave
                        // the still-running session fully usable.
                        let _ = state.blobs.deactivate(&session);
                        let _ = state.files.deactivate(&session);
                        // Stop first, forget second. `stop` joins the encode
                        // thread, and that thread drains what is still
                        // queued -- a `Snapshot` from the last detach, a
                        // frame -- before it exits, re-putting the picture.
                        // Removed before the join, it would come back for a
                        // same-name successor to serve until its first
                        // frame (see `bridge::tests::
                        // a_snapshot_queued_before_stop_lands_so_removal_must_follow_the_join`).
                        state.bridges.stop(&session);
                        state.thumbnails.remove(&session);
                        ResponseResult::Ack
                    })
                    .map_err(ApiFailure::from)
            }
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
        fail_terminate: bool,
        /// When set, `spawn` succeeds: it hands out pids and fabricates the
        /// readiness sockets under this runtime dir, the way the supervisor
        /// tests' runner does, so a `Run` can reach its success path.
        spawn_root: Option<std::path::PathBuf>,
    }

    impl NoopRunner {
        fn spawning(runtime_dir: std::path::PathBuf) -> Self {
            Self {
                spawn_root: Some(runtime_dir),
                ..Self::default()
            }
        }
    }

    impl ProcessRunner for NoopRunner {
        fn spawn(&self, spec: ProcessSpec) -> io::Result<u32> {
            let Some(runtime_dir) = &self.spawn_root else {
                return Err(io::Error::other("spawn is not used by this test"));
            };
            let mut alive = self.alive.lock().unwrap();
            let pid = alive.iter().next_back().map_or(100, |last| last + 1);
            alive.insert(pid);
            std::fs::create_dir_all(runtime_dir)?;
            for arg in &spec.args {
                if let Some(display) = arg.strip_prefix("--wayland-display=") {
                    std::fs::write(runtime_dir.join(display), b"")?;
                }
                if let Some(socket) = arg.strip_prefix("--socket=") {
                    std::fs::write(socket, b"")?;
                }
            }
            Ok(pid)
        }

        fn is_alive(&self, pid: u32) -> bool {
            self.alive.lock().unwrap().contains(&pid)
        }

        fn terminate(&self, _pid: u32, _force: bool) -> io::Result<()> {
            if self.fail_terminate {
                Err(io::Error::other("injected terminate failure"))
            } else {
                Ok(())
            }
        }
    }

    pub(crate) fn test_state(temp: &TempDir) -> ApiState<NoopRunner> {
        test_state_with_runner(temp, Arc::new(NoopRunner::default()))
    }

    fn test_state_with_runner(temp: &TempDir, runner: Arc<NoopRunner>) -> ApiState<NoopRunner> {
        let apps = AppIndex::from_apps([App {
            id: "firefox".into(),
            name: "Firefox".into(),
            icon: None,
            categories: vec!["Network".into()],
            exec: vec!["firefox".into()],
            terminal: false,
        }]);
        test_state_with(temp, runner, apps)
    }

    fn test_state_with(
        temp: &TempDir,
        runner: Arc<NoopRunner>,
        apps: AppIndex,
    ) -> ApiState<NoopRunner> {
        let registry = Registry::open(temp.path().join("registry.json")).unwrap();
        let supervisor = Supervisor::new(
            runner,
            Arc::new(Mutex::new(registry)),
            temp.path().join("runtime"),
            "wprsd",
        )
        .with_timeouts(
            Duration::from_millis(5),
            Duration::from_millis(5),
            Duration::from_millis(1),
        )
        .with_drop_root(temp.path().join("drops"));
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
        state.blobs.activate(name).unwrap();
        state.files.activate(name).unwrap();
    }

    fn add_stopped_session(state: &ApiState<NoopRunner>, name: &str) {
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
                status: SessionStatus::Stopped,
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
    async fn failed_kill_keeps_the_running_blob_namespace_live() {
        let temp = TempDir::new().unwrap();
        let runner = Arc::new(NoopRunner {
            alive: Mutex::new(BTreeSet::from([10, 11])),
            fail_terminate: true,
            spawn_root: None,
        });
        let state = test_state_with_runner(&temp, runner);
        add_running_session(&state, "work");

        let response = dispatch(
            &state,
            Request {
                request_id: 1,
                command: RequestCommand::Kill {
                    session: "work".into(),
                },
            },
        )
        .await;

        assert!(matches!(
            response.outcome,
            ResponseOutcome::Error {
                error: navette_protocol::ApiError {
                    code: ErrorCode::ProcessFailed,
                    ..
                }
            }
        ));
        assert!(
            state.blobs.begin_write("work", "image/png").is_ok(),
            "a failed kill must not revoke the still-running session's blob writers"
        );
        assert!(
            state
                .supervisor
                .registry()
                .lock()
                .unwrap()
                .get("work")
                .is_some()
        );
    }

    #[tokio::test]
    async fn kill_drops_the_sessions_thumbnail_with_its_other_state() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        add_running_session(&state, "work");
        state.thumbnails.put("work", test_thumbnail(8));
        state.thumbnails.put("other", test_thumbnail(8));

        let response = dispatch(
            &state,
            Request {
                request_id: 1,
                command: RequestCommand::Kill {
                    session: "work".into(),
                },
            },
        )
        .await;

        assert!(matches!(
            response.outcome,
            ResponseOutcome::Ok {
                result: ResponseResult::Ack
            }
        ));
        assert!(state.thumbnails.get("work").is_none());
        assert!(
            state.thumbnails.get("other").is_some(),
            "only the killed session's thumbnail goes"
        );
    }

    #[tokio::test]
    async fn kill_waits_for_the_sessions_lifecycle_transition_before_teardown() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        add_running_session(&state, "work");
        let transition = state.lock_session_lifecycle("work").await;
        let kill = dispatch(
            &state,
            Request {
                request_id: 1,
                command: RequestCommand::Kill {
                    session: "work".into(),
                },
            },
        );
        tokio::pin!(kill);

        assert!(
            timeout(Duration::from_millis(20), &mut kill).await.is_err(),
            "a Kill must not enter teardown while a same-name Run/rollback transition owns the lifecycle"
        );
        assert!(state.blobs.begin_write("work", "image/png").is_ok());
        drop(transition);

        let response = kill.await;
        assert!(matches!(
            response.outcome,
            ResponseOutcome::Ok {
                result: ResponseResult::Ack
            }
        ));
    }

    #[tokio::test]
    async fn websocket_lists_apps_and_echoes_request_id() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        add_running_session(&state, "work");
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
        add_running_session(&state, "work");
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
        state.blobs = BlobStore::with_limits(temp.path().join("limited-blobs"), 4, 6, 16);
        add_running_session(&state, "work");
        add_stopped_session(&state, "stopped");
        let token = state.auth.render();
        let app = router(state);

        let unknown_upload = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/sessions/unknown/blobs")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "image/png")
            .body(axum::body::Body::from("png"))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(unknown_upload).await.unwrap().status(),
            StatusCode::NOT_FOUND,
            "untrusted route names must not create blob directories"
        );
        assert!(
            !temp.path().join("limited-blobs/unknown").exists(),
            "an unknown session must not gain a blob namespace"
        );
        let stopped_upload = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/sessions/stopped/blobs")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "image/png")
            .body(axum::body::Body::from("png"))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(stopped_upload).await.unwrap().status(),
            StatusCode::NOT_FOUND,
            "a stopped registry record is not an active blob namespace"
        );
        assert!(
            !temp.path().join("limited-blobs/stopped").exists(),
            "a non-live session must not gain a blob namespace"
        );

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

    #[test]
    fn daemon_restart_preserves_delivered_files_for_a_live_session() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        add_running_session(&state, "work");
        let preflight = state
            .files
            .preflight(
                "work",
                FilePreflight {
                    name: "report.pdf".into(),
                    mime: "application/pdf".into(),
                    size: 4,
                },
            )
            .unwrap();
        let mut upload = state
            .files
            .begin_upload("work", &preflight.transfer_id)
            .unwrap();
        upload.write_chunk(b"data").unwrap();
        upload.finish().unwrap();
        state
            .files
            .materialize("work", &preflight.transfer_id)
            .unwrap();
        let path = state
            .files
            .drop_dir("work")
            .join(&preflight.transfer_id)
            .join("report.pdf");

        let _restarted = ApiState::new(
            Arc::clone(&state.apps),
            Arc::clone(&state.supervisor),
            Arc::new(navette_auth::AuthToken::generate()),
        );
        assert_eq!(std::fs::read(path).unwrap(), b"data");
    }

    #[test]
    fn handler_liveness_lease_rejects_a_preflight_after_kill_and_name_reuse() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        add_running_session(&state, "work");

        // This is the exact order in `create_file`: capture the file-store
        // incarnation, observe the old running registry record, then allow a
        // Kill+Run lifecycle transition before reservation creation.
        let lease = live_file_lease(&state, "work").unwrap();
        state.files.deactivate("work").unwrap();
        state.files.activate("work").unwrap();

        assert!(matches!(
            state.files.preflight_with_lease(
                &lease,
                FilePreflight {
                    name: "stale.pdf".into(),
                    mime: "application/pdf".into(),
                    size: 4,
                },
            ),
            Err(FileTransferError::SessionNotLive)
        ));
    }

    #[tokio::test]
    async fn file_routes_require_auth_reserve_exact_bytes_and_materialize_off_request_path() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        add_running_session(&state, "work");
        let token = state.auth.render();
        let app = router(state);

        let unauthenticated = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/sessions/work/files")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"name":"report.pdf","mime":"application/pdf","size":4}"#,
            ))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(unauthenticated).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let preflight = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/sessions/work/files")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"name":"report.pdf","mime":"application/pdf","size":4}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(preflight).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let preflight: crate::file_transfers::FilePreflightResponse = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(preflight.upload_url.ends_with("/content"));

        let wrong_length = axum::http::Request::builder()
            .method("PUT")
            .uri(&preflight.upload_url)
            .header("authorization", format!("Bearer {token}"))
            .header("content-length", "3")
            .body(axum::body::Body::from("data"))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(wrong_length).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );

        let upload = axum::http::Request::builder()
            .method("PUT")
            .uri(&preflight.upload_url)
            .header("authorization", format!("Bearer {token}"))
            .header("content-length", "4")
            .body(axum::body::Body::from("data"))
            .unwrap();
        let response = app.clone().oneshot(upload).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let status = axum::http::Request::builder()
            .uri(format!("/v1/sessions/work/files/{}", preflight.transfer_id))
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.clone().oneshot(status).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let status: crate::file_transfers::FileTransferStatus = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            status.state,
            crate::file_transfers::FileTransferState::Queued
                | crate::file_transfers::FileTransferState::Materializing
                | crate::file_transfers::FileTransferState::Delivered
        ));
    }

    #[tokio::test]
    async fn file_routes_cancel_untrusted_names_and_non_live_sessions() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        add_running_session(&state, "work");
        let token = state.auth.render();
        let app = router(state);
        let payload = r#"{"name":"report.pdf","mime":"application/pdf","size":4}"#;

        let traversal = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/sessions/work/files")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"name":"../report","mime":"application/pdf","size":4}"#,
            ))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(traversal).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        let missing_session = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/sessions/missing/files")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(payload))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(missing_session).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );

        let create = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/sessions/work/files")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(payload))
            .unwrap();
        let response = app.clone().oneshot(create).await.unwrap();
        let preflight: crate::file_transfers::FilePreflightResponse = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let cancel = axum::http::Request::builder()
            .method("DELETE")
            .uri(format!("/v1/sessions/work/files/{}", preflight.transfer_id))
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(cancel).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
    }

    /// The upload is held open through the store handle, exactly as a PUT
    /// whose body is still streaming holds it, so the DELETE below races a
    /// real in-flight upload rather than a finished one.
    #[tokio::test]
    async fn delete_wins_over_an_in_flight_put() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        add_running_session(&state, "work");
        let token = state.auth.render();
        let app = router(state.clone());

        let create = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/sessions/work/files")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"name":"report.pdf","mime":"application/pdf","size":4}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(create).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let preflight: crate::file_transfers::FilePreflightResponse = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let mut upload = state
            .files
            .begin_upload("work", &preflight.transfer_id)
            .unwrap();
        upload.write_chunk(b"da").unwrap();

        let cancel = axum::http::Request::builder()
            .method("DELETE")
            .uri(format!("/v1/sessions/work/files/{}", preflight.transfer_id))
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(cancel).await.unwrap().status(),
            StatusCode::NO_CONTENT,
            "DELETE must be authoritative while the PUT body is still streaming"
        );

        assert!(matches!(
            upload.write_chunk(b"ta"),
            Err(FileTransferError::Cancelled)
        ));
        let retry = axum::http::Request::builder()
            .method("PUT")
            .uri(&preflight.upload_url)
            .header("authorization", format!("Bearer {token}"))
            .header("content-length", "4")
            .body(axum::body::Body::from("data"))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(retry).await.unwrap().status(),
            StatusCode::CONFLICT,
            "a cancelled transfer cannot be re-uploaded"
        );
        let status = axum::http::Request::builder()
            .uri(format!("/v1/sessions/work/files/{}", preflight.transfer_id))
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(status).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let status: crate::file_transfers::FileTransferStatus = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            status.state,
            crate::file_transfers::FileTransferState::Cancelled
        );
    }

    /// Drives a real streaming PUT: the body yields one chunk, the DELETE
    /// lands while the handler waits for the next, and the second chunk makes
    /// the handler itself answer 409 rather than a later request.
    #[tokio::test]
    async fn put_streaming_after_delete_gets_conflict() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        add_running_session(&state, "work");
        let token = state.auth.render();
        let app = router(state);

        let create = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/sessions/work/files")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"name":"report.pdf","mime":"application/pdf","size":4}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(create).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let preflight: crate::file_transfers::FilePreflightResponse = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap(),
        )
        .unwrap();

        let (sender, receiver) = tokio::sync::mpsc::channel::<axum::body::Bytes>(1);
        let body = axum::body::Body::from_stream(futures_util::stream::unfold(
            receiver,
            |mut receiver| async move {
                receiver
                    .recv()
                    .await
                    .map(|chunk| (Ok::<_, std::convert::Infallible>(chunk), receiver))
            },
        ));
        let put = axum::http::Request::builder()
            .method("PUT")
            .uri(&preflight.upload_url)
            .header("authorization", format!("Bearer {token}"))
            .header("content-length", "4")
            .body(body)
            .unwrap();
        let cancel = axum::http::Request::builder()
            .method("DELETE")
            .uri(format!("/v1/sessions/work/files/{}", preflight.transfer_id))
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .unwrap();

        let drive = async {
            sender
                .send(axum::body::Bytes::from_static(b"da"))
                .await
                .unwrap();
            // The bounded channel hands out its next permit only once the
            // handler has taken the first chunk, which happens after its
            // `begin_upload`, so the DELETE below races a live upload.
            let permit = sender.reserve().await.unwrap();
            let cancelled = app.clone().oneshot(cancel).await.unwrap();
            permit.send(axum::body::Bytes::from_static(b"ta"));
            drop(sender);
            cancelled.status()
        };
        let (upload, cancelled) = tokio::join!(app.clone().oneshot(put), drive);
        assert_eq!(
            cancelled,
            StatusCode::NO_CONTENT,
            "DELETE must succeed while the PUT body is mid-stream"
        );
        assert_eq!(
            upload.unwrap().status(),
            StatusCode::CONFLICT,
            "the streaming PUT must end with 409 once cancelled"
        );

        let status = axum::http::Request::builder()
            .uri(format!("/v1/sessions/work/files/{}", preflight.transfer_id))
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(status).await.unwrap();
        let status: crate::file_transfers::FileTransferStatus = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            status.state,
            crate::file_transfers::FileTransferState::Cancelled
        );
    }

    fn wake_request() -> axum::http::request::Builder {
        axum::http::Request::builder()
            .method("POST")
            .uri("/v1/wake")
            .header("content-type", "application/json")
    }

    #[test]
    fn wake_requests_default_the_broadcast_address_and_port() {
        let target =
            parse_wake_request(br#"{"mac":"AA-BB-CC-DD-EE-FF"}"#, DEFAULT_BROADCAST).unwrap();
        assert_eq!(
            target,
            WakeTarget {
                mac: "aa:bb:cc:dd:ee:ff".parse().unwrap(),
                broadcast: Ipv4Addr::BROADCAST,
                port: 9,
            }
        );
        // The default is whatever the daemon was started with, not a constant
        // baked in here: `--wake-broadcast` exists for multi-homed hosts whose
        // routing table sends 255.255.255.255 out the wrong interface.
        let configured = parse_wake_request(
            br#"{"mac":"aabbccddeeff"}"#,
            Ipv4Addr::new(192, 168, 1, 255),
        )
        .unwrap();
        assert_eq!(configured.broadcast, Ipv4Addr::new(192, 168, 1, 255));
        let explicit = parse_wake_request(
            br#"{"mac":"aabbccddeeff","broadcast":"10.0.0.255","port":7}"#,
            DEFAULT_BROADCAST,
        )
        .unwrap();
        assert_eq!(explicit.broadcast, Ipv4Addr::new(10, 0, 0, 255));
        assert_eq!(explicit.port, 7);
    }

    #[test]
    fn wake_requests_may_only_name_lan_broadcast_targets() {
        // Subnet-directed broadcasts are the fix for multi-homed relays and
        // cannot be delivered to a loopback receiver in a test, so the parse
        // is what gets pinned here.
        for accepted in [
            "192.168.1.255",
            "10.0.0.255",
            "172.16.4.255",
            "100.100.1.255",
        ] {
            let body = format!(r#"{{"mac":"aabbccddeeff","broadcast":"{accepted}"}}"#);
            assert!(
                parse_wake_request(body.as_bytes(), DEFAULT_BROADCAST).is_ok(),
                "{accepted} must be accepted"
            );
        }
        for rejected in ["8.8.8.8", "203.0.113.255", "0.0.0.0", "100.63.255.255"] {
            let body = format!(r#"{{"mac":"aabbccddeeff","broadcast":"{rejected}"}}"#);
            let reason = parse_wake_request(body.as_bytes(), DEFAULT_BROADCAST).unwrap_err();
            assert!(
                reason.contains("100.64.0.0/10"),
                "{rejected} must be rejected naming the rule, got {reason:?}"
            );
        }
    }

    #[tokio::test]
    async fn wake_route_refuses_a_public_broadcast_target_and_names_the_rule() {
        let (app, token) = tests_support::test_router_with_token();
        let request = wake_request()
            .header("authorization", format!("Bearer {}", token.render()))
            .body(axum::body::Body::from(
                r#"{"mac":"aa:bb:cc:dd:ee:ff","broadcast":"8.8.8.8"}"#,
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("100.64.0.0/10"),
            "the 400 must say which addresses are allowed: {body:?}"
        );
    }

    #[tokio::test]
    async fn wake_route_answers_429_while_both_relay_slots_are_held() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        let token = state.auth.render();
        // Deterministic contention: hold the permits through the state handle
        // rather than racing requests, since a UDP send completes at once.
        let first = Arc::clone(&state.wake_slots).try_acquire_owned().unwrap();
        let _second = Arc::clone(&state.wake_slots).try_acquire_owned().unwrap();
        assert!(
            Arc::clone(&state.wake_slots).try_acquire_owned().is_err(),
            "the relay must offer exactly two slots"
        );
        let app = router(state);
        let body = r#"{"mac":"aa:bb:cc:dd:ee:ff","broadcast":"127.0.0.1","port":9}"#;

        let saturated = wake_request()
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(saturated).await.unwrap().status(),
            StatusCode::TOO_MANY_REQUESTS
        );

        drop(first);
        let receiver = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let port = receiver.local_addr().unwrap().port();
        let body =
            format!(r#"{{"mac":"aa:bb:cc:dd:ee:ff","broadcast":"127.0.0.1","port":{port}}}"#);
        let released = wake_request()
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        assert_eq!(
            app.oneshot(released).await.unwrap().status(),
            StatusCode::NO_CONTENT,
            "a released slot must be reusable"
        );
        let mut buffer = [0; 256];
        assert_eq!(receiver.recv_from(&mut buffer).unwrap().0, 102);
    }

    #[tokio::test]
    async fn wake_route_uses_the_configured_default_broadcast_when_the_request_omits_it() {
        let receiver = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let port = receiver.local_addr().unwrap().port();
        let temp = TempDir::new().unwrap();
        // This is what the phone does: it never sends `broadcast`, so a
        // multi-homed relay needs the daemon-side default to point at its LAN.
        let state = test_state(&temp).with_wake_broadcast(Ipv4Addr::LOCALHOST);
        let token = state.auth.render();
        let app = router(state);

        let body = format!(r#"{{"mac":"aa:bb:cc:dd:ee:ff","port":{port}}}"#);
        let request = wake_request()
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
        let mut buffer = [0; 256];
        let (received, _) = receiver.recv_from(&mut buffer).unwrap();
        let mac: MacAddress = "aa:bb:cc:dd:ee:ff".parse().unwrap();
        assert_eq!(&buffer[..received], &navette_wake::magic_packet(mac));
    }

    #[tokio::test]
    async fn wake_route_sits_behind_the_auth_and_origin_guards() {
        let (app, token) = tests_support::test_router_with_token();
        let body = r#"{"mac":"aa:bb:cc:dd:ee:ff","broadcast":"127.0.0.1","port":9}"#;

        let unauthenticated = wake_request().body(axum::body::Body::from(body)).unwrap();
        assert_eq!(
            app.clone().oneshot(unauthenticated).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let wrong_token = wake_request()
            .header("authorization", "Bearer 0000000000000000000000000")
            .body(axum::body::Body::from(body))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(wrong_token).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        // guard.rs owns the origin loop for the other routes; this pins the
        // same protection for the wake route without editing that file.
        let from_a_browser = wake_request()
            .header("authorization", format!("Bearer {}", token.render()))
            .header("origin", "https://evil.example")
            .body(axum::body::Body::from(body))
            .unwrap();
        assert_eq!(
            app.oneshot(from_a_browser).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn wake_route_answers_every_malformed_request_with_400() {
        let (app, token) = tests_support::test_router_with_token();
        let bearer = format!("Bearer {}", token.render());

        for (label, body) in [
            ("invalid mac", r#"{"mac":"nope"}"#),
            ("port 0", r#"{"mac":"aa:bb:cc:dd:ee:ff","port":0}"#),
            (
                "port out of range",
                r#"{"mac":"aa:bb:cc:dd:ee:ff","port":70000}"#,
            ),
            (
                "port as string",
                r#"{"mac":"aa:bb:cc:dd:ee:ff","port":"9"}"#,
            ),
            (
                "broadcast not an IPv4 literal",
                r#"{"mac":"aa:bb:cc:dd:ee:ff","broadcast":"192.168.1"}"#,
            ),
            (
                "broadcast is IPv6",
                r#"{"mac":"aa:bb:cc:dd:ee:ff","broadcast":"ff02::1"}"#,
            ),
            ("missing mac", r#"{"broadcast":"255.255.255.255"}"#),
            ("not an object", r#"["aa:bb:cc:dd:ee:ff"]"#),
        ] {
            let request = wake_request()
                .header("authorization", &bearer)
                .body(axum::body::Body::from(body))
                .unwrap();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::BAD_REQUEST,
                "{label}"
            );
        }

        // No Content-Type and no JSON at all: `Json<_>` would say 415 here,
        // and the contract says 400.
        let not_json = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/wake")
            .header("authorization", &bearer)
            .body(axum::body::Body::from("wake up"))
            .unwrap();
        assert_eq!(
            app.oneshot(not_json).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn wake_route_relays_a_magic_packet_to_the_requested_target() {
        let receiver = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let port = receiver.local_addr().unwrap().port();
        let (app, token) = tests_support::test_router_with_token();

        let body =
            format!(r#"{{"mac":"AA-BB-CC-DD-EE-FF","broadcast":"127.0.0.1","port":{port}}}"#);
        let request = wake_request()
            .header("authorization", format!("Bearer {}", token.render()))
            .body(axum::body::Body::from(body))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            axum::body::to_bytes(response.into_body(), 16)
                .await
                .unwrap()
                .is_empty()
        );

        let mut buffer = [0; 256];
        let (received, _) = receiver.recv_from(&mut buffer).unwrap();
        let mac: MacAddress = "aa:bb:cc:dd:ee:ff".parse().unwrap();
        assert_eq!(received, 102);
        assert_eq!(&buffer[..received], &navette_wake::magic_packet(mac));
    }

    fn test_thumbnail(len: usize) -> navette_bridge::Thumbnail {
        navette_bridge::Thumbnail {
            jpeg: vec![0xd8; len],
            width: 320,
            height: 180,
        }
    }

    fn get_with_bearer(uri: &str, token: &str) -> axum::http::request::Builder {
        axum::http::Request::builder()
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
    }

    #[tokio::test]
    async fn thumbnail_route_requires_auth_and_revalidates_the_latest_snapshot_by_etag() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        add_running_session(&state, "work");
        add_stopped_session(&state, "stopped");
        let token = state.auth.render();
        let thumbnails = state.thumbnails.clone();
        let app = router(state);

        let unauthenticated = axum::http::Request::builder()
            .uri("/v1/sessions/work/thumbnail")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(unauthenticated).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        for missing in [
            "/v1/sessions/nope/thumbnail",
            "/v1/sessions/stopped/thumbnail",
            "/v1/sessions/work/thumbnail",
        ] {
            let request = get_with_bearer(missing, &token)
                .body(axum::body::Body::empty())
                .unwrap();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::NOT_FOUND,
                "{missing}: unknown, stopped, and not-yet-snapshotted sessions all 404"
            );
        }

        thumbnails.put("work", test_thumbnail(16));
        let request = get_with_bearer("/v1/sessions/work/thumbnail", &token)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "image/jpeg");
        assert_eq!(response.headers()["content-length"], "16");
        assert_eq!(response.headers()["cache-control"], "no-cache");
        let etag = response.headers()["etag"].to_str().unwrap().to_owned();
        assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap(),
            vec![0xd8; 16]
        );

        let revalidate = get_with_bearer("/v1/sessions/work/thumbnail", &token)
            .header("if-none-match", &etag)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.clone().oneshot(revalidate).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()["etag"], etag.as_str());
        assert_eq!(response.headers()["cache-control"], "no-cache");
        assert!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .is_empty()
        );

        // A newer snapshot (different length, so a different etag even in
        // the same millisecond) makes the client's validator stale.
        thumbnails.put("work", test_thumbnail(24));
        let stale = get_with_bearer("/v1/sessions/work/thumbnail", &token)
            .header("if-none-match", &etag)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(stale).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_ne!(response.headers()["etag"], etag.as_str());
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .len(),
            24
        );
    }

    #[test]
    fn if_none_match_accepts_lists_weak_tags_and_the_wildcard() {
        let mut headers = HeaderMap::new();
        assert!(!if_none_match_matches(&headers, "\"a\""));

        headers.insert("if-none-match", "\"x\", W/\"a\"".parse().unwrap());
        assert!(if_none_match_matches(&headers, "\"a\""));
        assert!(if_none_match_matches(&headers, "\"x\""));
        assert!(!if_none_match_matches(&headers, "\"b\""));
        assert!(
            !if_none_match_matches(&headers, "a"),
            "quotes are part of the tag"
        );

        headers.insert("if-none-match", "*".parse().unwrap());
        assert!(if_none_match_matches(&headers, "\"anything\""));
    }

    /// A `.desktop` naming `Icon=fooapp` next to a `hicolor/64x64` PNG under
    /// a private data root, so the test never depends on the host's themes.
    fn state_with_icon_index(temp: &TempDir) -> (ApiState<NoopRunner>, Vec<u8>) {
        let apps_dir = temp.path().join("applications");
        let data_dir = temp.path().join("share");
        std::fs::create_dir_all(&apps_dir).unwrap();
        for (file, icon) in [("foo.desktop", "Icon=fooapp\n"), ("bare.desktop", "")] {
            std::fs::write(
                apps_dir.join(file),
                format!("[Desktop Entry]\nType=Application\nName={file}\nExec=/bin/echo\n{icon}"),
            )
            .unwrap();
        }
        let png = data_dir.join("icons/hicolor/64x64/apps/fooapp.png");
        std::fs::create_dir_all(png.parent().unwrap()).unwrap();
        let bytes = b"\x89PNG\r\n\x1a\nfake".to_vec();
        std::fs::write(&png, &bytes).unwrap();
        let apps = AppIndex::load_from_paths(
            vec![apps_dir],
            &[],
            &["kde".into()],
            "/bin".as_ref(),
            &[data_dir],
        );
        assert!(apps.get("foo").is_some() && apps.get("bare").is_some());
        (
            test_state_with(temp, Arc::new(NoopRunner::default()), apps),
            bytes,
        )
    }

    #[tokio::test]
    async fn icon_route_serves_the_resolved_png_and_revalidates_by_etag() {
        let temp = TempDir::new().unwrap();
        let (state, png) = state_with_icon_index(&temp);
        let token = state.auth.render();
        let app = router(state);

        let unauthenticated = axum::http::Request::builder()
            .uri("/v1/apps/foo/icon")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(unauthenticated).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let request = get_with_bearer("/v1/apps/foo/icon", &token)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "image/png");
        assert_eq!(response.headers()["cache-control"], "max-age=3600");
        assert_eq!(
            response.headers()["content-length"],
            png.len().to_string().as_str()
        );
        let etag = response.headers()["etag"].to_str().unwrap().to_owned();
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap(),
            png
        );

        let revalidate = get_with_bearer("/v1/apps/foo/icon", &token)
            .header("if-none-match", &etag)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.clone().oneshot(revalidate).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()["etag"], etag.as_str());
        assert!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .is_empty()
        );

        for missing in ["/v1/apps/bare/icon", "/v1/apps/nope/icon"] {
            let request = get_with_bearer(missing, &token)
                .body(axum::body::Body::empty())
                .unwrap();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::NOT_FOUND,
                "{missing}: no resolvable PNG and unknown app both 404"
            );
        }
    }

    #[tokio::test]
    async fn icon_route_refuses_an_oversized_or_vanished_file() {
        let temp = TempDir::new().unwrap();
        let (state, _png) = state_with_icon_index(&temp);
        let token = state.auth.render();
        let path = state.apps.icon_path("foo").unwrap().to_path_buf();
        let app = router(state);

        std::fs::write(&path, vec![0u8; MAX_ICON_BYTES as usize + 1]).unwrap();
        let request = get_with_bearer("/v1/apps/foo/icon", &token)
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::NOT_FOUND,
            "a file over the icon limit is not read, let alone served"
        );

        std::fs::remove_file(&path).unwrap();
        let request = get_with_bearer("/v1/apps/foo/icon", &token)
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::NOT_FOUND,
            "an icon removed after index load 404s rather than erroring"
        );
    }

    /// Item 4a: the file the index resolved must actually be a PNG. A theme
    /// that ships something else under a `.png` name gets a placeholder on
    /// the phone, not a body the client would try to decode.
    #[tokio::test]
    async fn icon_route_refuses_a_file_without_a_png_signature() {
        let temp = TempDir::new().unwrap();
        let (state, _png) = state_with_icon_index(&temp);
        let token = state.auth.render();
        let path = state.apps.icon_path("foo").unwrap().to_path_buf();
        let app = router(state);

        std::fs::write(&path, b"<svg xmlns='http://www.w3.org/2000/svg'/>").unwrap();
        let request = get_with_bearer("/v1/apps/foo/icon", &token)
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }

    /// Item 1: a name can be reused only after its previous session is gone
    /// from the registry; whatever the store still holds under that name
    /// belongs to the dead session and must not be served as the new one's
    /// first picture.
    #[tokio::test]
    async fn run_starts_a_reused_name_with_no_stale_thumbnail() {
        let temp = TempDir::new().unwrap();
        let runner = Arc::new(NoopRunner::spawning(temp.path().join("runtime")));
        let state = test_state_with_runner(&temp, runner);
        state.thumbnails.put("work", test_thumbnail(8));

        let response = dispatch(
            &state,
            Request {
                request_id: 1,
                command: RequestCommand::Run {
                    app_id: "firefox".into(),
                    name: Some("work".into()),
                },
            },
        )
        .await;

        assert!(
            matches!(
                response.outcome,
                ResponseOutcome::Ok {
                    result: ResponseResult::Session { .. }
                }
            ),
            "{response:?}"
        );
        assert!(
            state.thumbnails.get("work").is_none(),
            "a stale entry under a reused name must be cleared before the new session serves"
        );
    }
}
