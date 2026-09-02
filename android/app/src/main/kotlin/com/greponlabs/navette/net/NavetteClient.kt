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
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
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
}

/**
 * One control-channel connection to a `navetted` host. Request/response
 * correlation is by `request_id`, since the control channel is one JSON
 * value per WebSocket text frame with no built-in ordering guarantee beyond
 * what `navetted`'s own single-threaded request handling provides -- see
 * `navette-protocol`'s `Request`/`Response` doc comment.
 *
 * Not thread-safety-tested beyond what `ConcurrentHashMap` and `OkHttp`'s
 * own listener-thread contract give for free; this is the first slice, not
 * the hardened version -- reconnection, backoff, and multi-host management
 * are explicitly out of scope here (see android/README.md and this
 * session's PR description).
 */
class NavetteClient(private val webSocketUrl: String) {
    private val httpClient =
        OkHttpClient.Builder()
            .pingInterval(20, TimeUnit.SECONDS)
            .build()

    private var webSocket: WebSocket? = null
    private val pending = ConcurrentHashMap<Long, CompletableDeferred<Response>>()
    private val nextRequestId = AtomicLong(1)

    private val _connectionState = MutableStateFlow<ConnectionState>(ConnectionState.Disconnected)
    val connectionState: StateFlow<ConnectionState> = _connectionState.asStateFlow()

    fun connect() {
        _connectionState.value = ConnectionState.Connecting
        val request =
            Request.Builder()
                .url(webSocketUrl)
                .addHeader("Sec-WebSocket-Protocol", CONTROL_WEBSOCKET_SUBPROTOCOL)
                .build()
        webSocket = httpClient.newWebSocket(request, listener)
    }

    /** Sends [command] and suspends until `navetted` answers it, by `request_id`. */
    suspend fun call(command: RequestCommand): Response {
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
        return deferred.await()
    }

    fun close() {
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
                _connectionState.value = ConnectionState.Failed(t.message ?: "connection failed")
                failAllPending(t)
            }

            override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                _connectionState.value = ConnectionState.Disconnected
                failAllPending(IllegalStateException("connection closed: $reason"))
            }
        }

    private companion object {
        const val NORMAL_CLOSURE = 1000
    }
}

fun controlWebSocketUrl(host: String, port: Int = 9417): String =
    "ws://$host:$port$CONTROL_WEBSOCKET_PATH"
