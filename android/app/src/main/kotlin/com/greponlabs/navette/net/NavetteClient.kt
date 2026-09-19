package com.greponlabs.navette.net

import com.greponlabs.navette.protocol.CONTROL_WEBSOCKET_PATH
import com.greponlabs.navette.protocol.CONTROL_WEBSOCKET_SUBPROTOCOL
import com.greponlabs.navette.protocol.ControlCodec
import com.greponlabs.navette.protocol.ProtocolException
import com.greponlabs.navette.protocol.RequestCommand
import com.greponlabs.navette.protocol.Response
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.withTimeout
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response as OkHttpResponse
import okhttp3.WebSocket
import okhttp3.WebSocketListener

sealed interface ConnectionState {
    data object Disconnected : ConnectionState

    data object Connecting : ConnectionState

    data object Connected : ConnectionState

    data class Failed(val reason: String) : ConnectionState

    /**
     * The daemon refused our token. Terminal: retrying cannot succeed until the
     * user pairs again, and retrying anyway produces a silent infinite loop
     * that looks to the user like a network problem.
     */
    data object Unauthorized : ConnectionState
}

/**
 * What [AppViewModel][com.greponlabs.navette.ui.AppViewModel] needs from a
 * control-channel connection. Exists so tests can inject a fake instead of
 * standing up real networking -- see `AppViewModelTest`.
 */
interface NavetteApi {
    val connectionState: StateFlow<ConnectionState>

    fun connect()

    suspend fun call(command: RequestCommand): Response

    fun close()
}

/**
 * One control-channel connection to a `navetted` host. Request/response
 * correlation is by `request_id`, since the control channel is one JSON
 * value per WebSocket text frame with no built-in ordering guarantee beyond
 * what `navetted`'s own single-threaded request handling provides -- see
 * `navette-protocol`'s `Request`/`Response` doc comment.
 *
 * **Thread-confined, not thread-safe**: `pending`/`nextRequestId` are
 * concurrency-safe types, but the plain `webSocket` field is not
 * synchronized. Every method here is expected to be called from a single
 * dispatcher (in practice, `AppViewModel`'s `viewModelScope`, which defaults
 * to `Dispatchers.Main.immediate`) -- calling `connect()`/`call()`/`close()`
 * from more than one dispatcher concurrently is not supported. This is the
 * first slice, not the hardened version -- reconnection, backoff, and
 * multi-host management are explicitly out of scope here (see
 * android/README.md and this session's PR description).
 */
class NavetteClient(private val webSocketUrl: String, private val token: String) : NavetteApi {
    private val httpClient =
        OkHttpClient.Builder()
            .pingInterval(20, TimeUnit.SECONDS)
            .build()

    private var webSocket: WebSocket? = null
    private val pending = ConcurrentHashMap<Long, CompletableDeferred<Response>>()
    private val nextRequestId = AtomicLong(1)

    private val _connectionState = MutableStateFlow<ConnectionState>(ConnectionState.Disconnected)
    override val connectionState: StateFlow<ConnectionState> = _connectionState.asStateFlow()

    override fun connect() {
        _connectionState.value = ConnectionState.Connecting
        val request =
            try {
                Request.Builder()
                    .url(webSocketUrl)
                    .addHeader("Sec-WebSocket-Protocol", CONTROL_WEBSOCKET_SUBPROTOCOL)
                    .addHeader("Authorization", "Bearer $token")
                    .build()
            } catch (error: IllegalArgumentException) {
                // A malformed webSocketUrl (okhttp throws IllegalArgumentException
                // for one) must surface as a Failed state, not crash the caller --
                // controlWebSocketUrl can't fully validate every host shape itself.
                _connectionState.value = ConnectionState.Failed(error.message ?: "invalid host")
                return
            }
        webSocket = httpClient.newWebSocket(request, listener)
    }

    /**
     * Sends [command] and suspends until `navetted` answers it, by
     * `request_id`, or [CALL_TIMEOUT_MS] elapses. Connection loss is already
     * covered by [failAllPending] -- this covers the other way a call can
     * hang forever: the connection stays healthy but `navetted` never
     * answers this particular request.
     */
    override suspend fun call(command: RequestCommand): Response {
        val requestId = nextRequestId.getAndIncrement()
        val deferred = CompletableDeferred<Response>()
        pending[requestId] = deferred

        val socket = webSocket
        if (socket == null) {
            pending.remove(requestId)
            throw IllegalStateException("not connected")
        }
        val sent = socket.send(ControlCodec.encodeRequest(requestId, command))
        if (!sent) {
            pending.remove(requestId)
            throw IllegalStateException("failed to send request $requestId")
        }
        return try {
            withTimeout(CALL_TIMEOUT_MS) { deferred.await() }
        } catch (error: TimeoutCancellationException) {
            throw IllegalStateException("request $requestId timed out waiting for a response")
        } finally {
            pending.remove(requestId)
        }
    }

    override fun close() {
        webSocket?.close(NORMAL_CLOSURE, "client closing")
        webSocket = null
        _connectionState.value = ConnectionState.Disconnected
        failAllPending(IllegalStateException("client closed"))
    }

    private fun failAllPending(cause: Throwable) {
        val requestIds = pending.keys.toList()
        for (requestId in requestIds) {
            pending.remove(requestId)?.completeExceptionally(cause)
        }
    }

    private val listener =
        object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: OkHttpResponse) {
                _connectionState.value = ConnectionState.Connected
            }

            override fun onMessage(webSocket: WebSocket, text: String) {
                val response =
                    try {
                        ControlCodec.decodeResponse(text)
                    } catch (error: ProtocolException) {
                        // A response this client cannot parse is not
                        // actionable against any pending call, so it is
                        // dropped rather than crashing the listener thread.
                        return
                    }
                pending.remove(response.requestId)?.complete(response)
            }

            override fun onFailure(webSocket: WebSocket, t: Throwable, response: OkHttpResponse?) {
                // NavetteClient has no reconnect loop of its own to stop
                // (see this class's doc comment), so distinguishing
                // Unauthorized here is purely about surfacing: the user must
                // see "pairing rejected", not a generic connection failure
                // they would otherwise blame on the network. When
                // reconnection is added here, it inherits the terminal rule
                // ReconnectPolicy already enforces for the media path.
                _connectionState.value =
                    if (response?.code == 401) {
                        ConnectionState.Unauthorized
                    } else {
                        ConnectionState.Failed(t.message ?: "connection failed")
                    }
                failAllPending(t)
            }

            override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                _connectionState.value = ConnectionState.Disconnected
                failAllPending(IllegalStateException("connection closed: $reason"))
            }
        }

    private companion object {
        const val NORMAL_CLOSURE = 1000
        const val CALL_TIMEOUT_MS = 15_000L
    }
}

/**
 * Bracket-wraps a bare IPv6 literal (e.g. a Tailscale address like
 * `fd7a:115c:a1e0::1`) so the authority is unambiguous -- hostnames and IPv4
 * addresses, which never contain more than one colon, pass through
 * unchanged. This can't validate every malformed [host] (one that already
 * smuggles in a port, for instance); [NavetteClient.connect] and
 * [MediaClient.connect] both guard against building an invalid [Request]
 * from whatever comes out of here.
 */
internal fun formatAuthorityHost(host: String): String =
    if (host.count { it == ':' } >= 2 && !host.startsWith("[")) "[$host]" else host

/**
 * The port `navetted` listens on by default. Shared by [controlWebSocketUrl]
 * and [mediaWebSocketUrl] as their default, and by manual pairing entry
 * (`ConnectScreen`) when the user has no reason to type anything but the
 * host -- there is one definition of "default" rather than two literals that
 * can drift apart.
 */
const val DEFAULT_NAVETTE_PORT = 9417

/** Builds the control-channel WebSocket URL for [host]:[port]. */
fun controlWebSocketUrl(host: String, port: Int = DEFAULT_NAVETTE_PORT): String =
    "ws://${formatAuthorityHost(host)}:$port$CONTROL_WEBSOCKET_PATH"

/**
 * Builds the media-channel WebSocket URL for [session] on [host]:[port].
 *
 * [session] is deliberately not percent-encoded: `navetted` validates every
 * session name against `[a-z0-9_-]{1,64}` before one can exist
 * (`validate_session_name`, `crates/navetted/src/registry.rs:267-281`), so
 * the only names that can reach here are already URL-path-safe.
 */
fun mediaWebSocketUrl(host: String, session: String, port: Int = DEFAULT_NAVETTE_PORT): String =
    "ws://${formatAuthorityHost(host)}:$port/v1/sessions/$session/media"

/** HTTP collection backing the session-scoped, client-to-guest file upload API. */
fun fileTransferCollectionUrl(mediaUrl: String): String =
    mediaUrl
        .replaceFirst("ws://", "http://")
        .replaceFirst("wss://", "https://")
        .removeSuffix("/media") + "/files"
