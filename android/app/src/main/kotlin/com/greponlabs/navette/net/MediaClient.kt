package com.greponlabs.navette.net

import android.util.Log
import java.util.concurrent.Semaphore
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.runBlocking
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response as OkHttpResponse
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString

/**
 * What one packet costs against the socket queue's byte budget, in KiB.
 *
 * A direct port of `client.rs:310-312`.
 *
 * **Must stay a pure function of the payload size.** `deliver` and
 * `nextPacket` compute the charge independently rather than carrying a permit
 * with the packet, so the moment this depends on anything else -- a header
 * field, a per-kind weighting, a value read at construction -- the two
 * disagree and the semaphore drifts, with nothing to catch it.
 *
 * Two properties carry the weight, and both are the reason this is a named
 * function rather than an inline expression -- it is the one piece of the
 * budget that is testable without a socket:
 *
 * - **Always at least one**, so a flood of empty packets is still bounded by
 *   the queue's own object capacity rather than costing nothing.
 * - **Clamped to [budgetKib]**, so a payload larger than the entire budget --
 *   which the protocol permits, at 16 MiB against an 8 MiB budget -- charges
 *   the whole thing instead of becoming unacquirable and stalling forever.
 *   That clamp is what makes "one oversized packet at a time" true.
 */
internal fun packetBudgetKib(packet: MediaPacket, budgetKib: Int): Int {
    val kib = (packet.payload.size + 1023) / 1024
    return kib.coerceIn(1, budgetKib)
}

/**
 * One media-channel connection to a session's `/v1/sessions/{name}/media`
 * endpoint. Binary frames decode into [MediaPacket]s and are handed to the
 * caller through [packets]; [MediaInput] goes back out as JSON text frames.
 *
 * Deliberately simpler than `crates/navette-viewer/src/client.rs`, which this
 * mirrors: that client hand-rolls an input queue and a drain loop because
 * tokio-tungstenite's sink blocks its caller. OkHttp's [WebSocket.send]
 * already enqueues writes internally and never blocks, so [sendInput] can be
 * called straight from a UI-thread touch callback with no queue of its own.
 *
 * **No reconnect and no subprotocol verification on open**, matching
 * [NavetteClient]'s own explicitly-deferred gaps -- a dropped media socket
 * surfaces as [ConnectionState.Failed]/[ConnectionState.Disconnected] and the
 * user re-attaches from the drawer.
 *
 * **Thread-confined, not thread-safe**, on the same terms as [NavetteClient]:
 * [connect] and [close] belong to a single dispatcher. [sendInput] -- and
 * [sendPing], which is built on it -- is the exception and is safe from any
 * thread, because it only touches OkHttp's own thread-safe [WebSocket.send].
 * [onPong] is different again: it is set from the connecting dispatcher
 * before [connect] but invoked on OkHttp's reader thread, so whatever it does
 * must itself be safe to run there.
 */
class MediaClient(private val webSocketUrl: String, private val token: String) {
    private val httpClient =
        OkHttpClient.Builder()
            // OkHttp fails the socket if a ping goes unanswered for a whole
            // interval, so this doubles as how fast a silently-dropped link is
            // noticed -- a dead tailnet route stops delivering frames but does
            // not close the TCP socket, and nothing else here would detect it.
            // The session screen retries on that failure, so a shorter interval
            // is what turns a frozen picture into a "Reconnecting..." within
            // seconds. OkHttp answers its own pings on this same reader
            // thread, which also runs onMessage -- and onMessage's onPong
            // hook now takes a lock shared with the main thread. That wait is
            // bounded by whatever section of [SessionController] holds the
            // lock, and every one of those is a brief field read or write,
            // never a codec teardown -- so a false timeout still tracks a
            // genuine [PING_INTERVAL_SECONDS]-long network stall, not
            // main-thread jank or a MediaCodec.release().
            .pingInterval(PING_INTERVAL_SECONDS, TimeUnit.SECONDS)
            .build()

    @Volatile
    private var webSocket: WebSocket? = null

    private val _packets = Channel<MediaPacket>(capacity = PACKET_QUEUE_CAPACITY)

    /**
     * The byte budget for [_packets], in KiB, mirroring `client.rs:101`'s
     * `Semaphore::new(PACKET_QUEUE_KIB)`. A queue bounded only by object count
     * is not a memory bound: the protocol permits a 16 MiB payload, so 32
     * queued packets is 512 MiB in the worst case a peer can construct.
     *
     * A real semaphore rather than a counter because *every* packet must
     * respect it, and the must-arrive kinds have to be able to **wait** for
     * room rather than either dropping or bypassing the cap. A counter can
     * only refuse; it cannot block for space.
     *
     * Permits are taken in [deliver] and returned in [nextPacket]. That
     * covers what sits in the socket queue only -- unlike `client.rs`, which
     * carries its permit to the end of the decode thread's work. That is
     * deliberate: a packet leaving here is immediately charged against the
     * decoder's own separate budget, so the two are sequential rather than
     * overlapping and neither needs to account for the other's holdings.
     */
    private val budget = Semaphore(PACKET_QUEUE_KIB)

    /** Set by [endStream] so a reader thread waiting for budget cannot park forever. */
    @Volatile
    private var closed = false

    /**
     * Set when a keyframe has been asked for and not yet seen, so a sustained
     * overflow asks once rather than once per dropped packet. This is the same
     * "Full means a request is already pending" property `client.rs` gets from
     * a one-slot channel (`client.rs:106-108`).
     */
    private val keyframePending = AtomicBoolean(false)

    private val _connectionState = MutableStateFlow<ConnectionState>(ConnectionState.Disconnected)
    val connectionState: StateFlow<ConnectionState> = _connectionState.asStateFlow()

    /** Mirrors [NavetteClient.connect]'s guarded shape: a malformed URL must not crash the caller. */
    fun connect() {
        _connectionState.value = ConnectionState.Connecting
        val request =
            try {
                Request.Builder()
                    .url(webSocketUrl)
                    .addHeader("Sec-WebSocket-Protocol", MEDIA_WEBSOCKET_SUBPROTOCOL)
                    .addHeader("Authorization", "Bearer $token")
                    .build()
            } catch (error: IllegalArgumentException) {
                _connectionState.value = ConnectionState.Failed(error.message ?: "invalid host")
                return
            }
        webSocket = httpClient.newWebSocket(request, listener)
    }

    /**
     * Sends [input] if it is in range, and reports whether it reached the
     * socket. Validating first mirrors `MediaClient::send_input` in
     * `client.rs:150-156`: the bridge rejects out-of-range input anyway, and
     * never sending it keeps a local bug from reading as a protocol
     * violation on the wire.
     */
    fun sendInput(input: MediaInput): Boolean {
        val invalid = input.validate()
        if (invalid != null) {
            Log.w(TAG, "refusing to send out-of-range input: $invalid")
            return false
        }
        val socket = webSocket ?: return logDropped(input, "no socket")
        return socket.send(mediaJson.encodeToString(MediaInput.serializer(), input)) ||
            logDropped(input, "socket declined the frame")
    }

    /**
     * Debug, not warn: every caller already treats a dropped send as
     * unsurprising during a known-bad connection (the entire reason
     * [SessionController] retries), so this is a breadcrumb for whoever
     * investigates a specific missing input next, not an operational alert.
     * Always returns `false`, so callers can tail-call it as their failure path.
     */
    private fun logDropped(input: MediaInput, reason: String): Boolean {
        // Variant name only: MediaInput.SetClipboard is a data class, so the
        // naive "$input" would render the user's clipboard text into logcat.
        Log.d(TAG, "dropped ${input::class.simpleName}: $reason")
        return false
    }

    /**
     * The next packet, or `null` once the connection has ended.
     *
     * A method rather than an exposed `ReceiveChannel` because this is where
     * [queuedBytes] is credited back -- the byte budget only works if the
     * client sees each packet leave the queue.
     */
    suspend fun nextPacket(): MediaPacket? {
        val packet = _packets.receiveCatching().getOrNull() ?: return null
        // The charge is recomputed rather than carried with the packet: it is
        // a pure function of the payload size, so it cannot disagree.
        budget.release(packetBudgetKib(packet, PACKET_QUEUE_KIB))
        if (packet.header.flags.keyframe) keyframePending.set(false)
        return packet
    }

    fun close() {
        webSocket?.close(NORMAL_CLOSURE, "client closing")
        webSocket = null
        _connectionState.value = ConnectionState.Disconnected
        endStream()
        // This client is single-use and owns its OkHttpClient; a reconnect
        // builds a new one. Release this one's dispatcher threads and pooled
        // connections now rather than leaving them to OkHttp's 60s / 5min idle
        // reclaim to accumulate across retries. The WebSocket's own writer
        // runs on OkHttp's task runner, not this executor, so the close frame
        // above still goes out.
        httpClient.dispatcher.executorService.shutdown()
        httpClient.connectionPool.evictAll()
    }

    /**
     * Ends packet delivery, from every path that can end it.
     *
     * Setting [closed] is not optional bookkeeping alongside closing the
     * channel: a reader thread parked in [acquireBudget] waits on permits the
     * consumer returns, and once the consumer has stopped nothing will return
     * any. [closed] is the only thing that releases it, so a path that closed
     * the channel without setting this would leak that thread for the life of
     * the process.
     */
    private fun endStream() {
        closed = true
        _packets.close()
    }

    /**
     * Hands one packet to the consumer.
     *
     * `client.rs` gets true backpressure here by awaiting a bounded tokio
     * channel. OkHttp's reader callback is not a coroutine, so the closest
     * faithful equivalent is: try the bounded queue first, and when it is
     * full, distinguish what may be lost from what may not. A dropped
     * [MediaKind.VIDEO] packet is recoverable -- the decoder recovers on the
     * next keyframe -- but a dropped [MediaKind.STREAM_CONFIG] means the gate
     * never adopts a primary stream and the screen stays black with no path
     * back, and a dropped [MediaKind.STREAM_END] leaves the screen live over
     * a stream that has gone. Those block this reader thread instead, which
     * is real backpressure on the socket rather than a silent loss.
     */
    private fun deliver(packet: MediaPacket) {
        val kib = packetBudgetKib(packet, PACKET_QUEUE_KIB)
        val mustArrive =
            packet.header.kind == MediaKind.STREAM_CONFIG || packet.header.kind == MediaKind.STREAM_END

        if (!acquireBudget(kib, mustArrive)) {
            Log.w(TAG, "dropping a ${packet.header.kind} packet; the consumer is not keeping up")
            if (!mustArrive) requestKeyframe()
            return
        }

        // Budget is held from here on; every path that does not hand the
        // packet to the queue must give it back.
        if (mustArrive) {
            // Losing one of these has no recovery path -- without a
            // StreamConfig the gate never adopts a primary stream, and without
            // a StreamEnd the screen stays live over a stream that has gone.
            // Blocking this reader thread is real backpressure on the socket,
            // which is exactly what `client.rs:313-316` does for every packet.
            runBlocking {
                runCatching { _packets.send(packet) }
                    .onFailure {
                        budget.release(kib)
                        Log.d(TAG, "consumer gone; dropping ${packet.header.kind}")
                    }
            }
            return
        }

        // Video re-primes from the next keyframe, and metrics are discarded by
        // the consumer anyway -- neither is worth parking the socket for.
        if (!_packets.trySend(packet).isSuccess) {
            budget.release(kib)
            Log.w(TAG, "dropping a ${packet.header.kind} packet; the packet queue is full")
            requestKeyframe()
        }
    }

    /**
     * Per-kind payload bounds, applied on top of the protocol's own.
     *
     * `MediaPacket.decode` only enforces the shared 16 MiB ceiling, which is
     * far larger than any kind but video actually needs. Refusing an
     * implausible one here keeps a well-formed but abusive packet from ever
     * reaching the queue, where the must-arrive kinds cannot be dropped.
     */
    private fun plausibleForKind(packet: MediaPacket): Boolean {
        val size = packet.payload.size
        val plausible =
            when (packet.header.kind) {
                MediaKind.STREAM_CONFIG -> size <= MAX_STREAM_CONFIG_BYTES
                MediaKind.STREAM_END -> size == 0
                MediaKind.VIDEO, MediaKind.METRICS -> true
            }
        if (!plausible) {
            Log.w(TAG, "discarding a ${packet.header.kind} packet with an implausible $size-byte payload")
        }
        return plausible
    }

    /**
     * Takes [kib] permits, waiting only for a packet that must not be lost.
     *
     * The wait is a bounded poll rather than a plain `acquire` so [close] can
     * free a parked reader thread: nothing else would wake a thread blocked on
     * a semaphore whose permits are held by a consumer that has gone away.
     */
    private fun acquireBudget(kib: Int, mustArrive: Boolean): Boolean {
        if (!mustArrive) return budget.tryAcquire(kib)
        while (!closed) {
            if (budget.tryAcquire(kib, BUDGET_WAIT_MS, TimeUnit.MILLISECONDS)) return true
        }
        return false
    }

    /**
     * Asks the bridge for a keyframe after a drop, at most once until one
     * arrives. Recovery depends on this: a dropped access unit leaves the
     * decoder missing a reference the frames after it need, and nothing else
     * produces a fresh keyframe before the encoder's own GOP cadence.
     *
     * Public because packets are dropped in two places -- here, when the
     * consumer falls behind, and in the decoder, when its input backlog
     * fills. Both must share this one gate, or a sustained backlog sends a
     * request per dropped unit, which is the flood the gate exists to stop.
     */
    fun requestKeyframe() {
        if (keyframePending.compareAndSet(false, true)) {
            sendInput(MediaInput.RequestKeyframe)
        }
    }

    /** Sends one ping. The caller owns the nonce, so it can time the answer. */
    fun sendPing(nonce: ULong) = sendInput(MediaInput.Ping(nonce))

    /**
     * Told about each pong, so a caller can time it against its own ping.
     * Set before [connect]; called on OkHttp's reader thread.
     */
    @Volatile
    var onPong: ((ULong) -> Unit)? = null

    /**
     * Told about each guest clipboard push. Same contract as [onPong]: set
     * before [connect], called on OkHttp's reader thread, so whatever it does
     * must itself be safe to run there. Never logged past here -- the text
     * is clipboard content.
     */
    @Volatile
    var onClipboard: ((String) -> Unit)? = null

    /** Descriptor for a guest image whose bytes must be fetched separately. */
    @Volatile
    var onClipboardBlob: ((BlobDescriptor) -> Unit)? = null

    private val listener =
        object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: OkHttpResponse) {
                _connectionState.value = ConnectionState.Connected
                // Mirrors client.rs:97 -- ask for a keyframe immediately so the
                // first picture does not wait for the encoder's own GOP cadence.
                sendInput(MediaInput.RequestKeyframe)
            }

            override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
                // Checked against ByteString.size, before toByteArray() copies
                // anything. OkHttp's frame reader imposes no size limit of its
                // own, so without this a single oversized frame -- or a
                // permessage-deflate bomb, which inflates before reaching
                // here -- would exhaust memory before MediaProtocol's own
                // bounds ever ran.
                if (bytes.size > MAX_FRAME_BYTES) {
                    Log.w(TAG, "discarding a ${bytes.size}-byte media frame; the protocol caps at $MAX_FRAME_BYTES")
                    return
                }
                val packet =
                    try {
                        MediaPacket.decode(bytes.toByteArray())
                    } catch (error: MediaDecodeException) {
                        // Matches receive()'s `Err(error) => ... true` in
                        // client.rs:318-321: a malformed packet is discarded,
                        // it never takes the connection down.
                        Log.w(TAG, "discarding malformed media packet: ${error.error}")
                        return
                    }
                if (!plausibleForKind(packet)) return
                deliver(packet)
            }

            override fun onMessage(webSocket: WebSocket, text: String) {
                // The bridge reports protocol problems as JSON text; they are
                // informational and must not take the connection down. An
                // unparseable body includes an old daemon's reply to a message
                // it does not know -- a ping, for one -- which is exactly why
                // this logs rather than fails.
                val reported =
                    runCatching { mediaJson.decodeFromString(MediaServerMessage.serializer(), text) }
                        .getOrNull()
                when (reported) {
                    is MediaServerMessage.Pong -> onPong?.invoke(reported.nonce)
                    is MediaServerMessage.Clipboard -> onClipboard?.invoke(reported.text)
                    is MediaServerMessage.ClipboardBlob -> onClipboardBlob?.invoke(reported.blob)
                    is MediaServerMessage.Error -> Log.w(TAG, "media server reported: $reported")
                    // Never `text` itself: an unparseable frame that was meant
                    // to be a Clipboard message has clipboard content
                    // embedded in this raw, undecoded body.
                    null -> Log.w(TAG, "media server sent an undecodable ${text.length}-char text frame")
                }
            }

            override fun onFailure(webSocket: WebSocket, t: Throwable, response: OkHttpResponse?) {
                // A rotated token otherwise produces an invisible infinite
                // reconnect loop: the phone spins forever while the daemon
                // refuses every attempt, read by the user as a network
                // problem. Distinguishing Unauthorized here is what lets
                // ReconnectPolicy.shouldRetry make that terminal.
                _connectionState.value =
                    if (response?.code == 401) {
                        ConnectionState.Unauthorized
                    } else {
                        ConnectionState.Failed(t.message ?: "connection failed")
                    }
                endStream()
            }

            /**
             * Completes the closing handshake. Without this the socket sits
             * half-open until a timeout, which is exactly the "silent hang"
             * the session screen must not show when `navetted` goes away
             * server-side.
             */
            override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                webSocket.close(NORMAL_CLOSURE, null)
            }

            override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                _connectionState.value = ConnectionState.Disconnected
                endStream()
            }
        }

    private companion object {
        const val TAG = "MediaClient"
        const val NORMAL_CLOSURE = 1000

        /**
         * How often the socket is pinged, and so the ceiling on how long a
         * silently-dropped link takes to surface as a failure the session
         * screen can retry. Lowered from a keepalive-only 20s once reconnect
         * existed: 5s detects a real drop within ~5-10s while staying well
         * clear of a false positive on a healthy link.
         */
        const val PING_INTERVAL_SECONDS = 5L

        /**
         * Matches `EVENT_QUEUE_CAPACITY` in `client.rs:23`. Deep enough to
         * absorb a burst while the decoder is busy, shallow enough that a
         * queue this long means something is genuinely wrong.
         */
        const val PACKET_QUEUE_CAPACITY = 32

        /**
         * The largest frame worth reading at all: a full header plus the
         * largest payload the protocol defines. Anything bigger is malformed
         * or hostile by definition, so it is refused before it is copied.
         */
        const val MAX_FRAME_BYTES = MEDIA_HEADER_LEN + MAX_MEDIA_PAYLOAD

        /**
         * Payload bytes allowed to sit queued for the consumer, in KiB,
         * mirroring `PACKET_QUEUE_KIB` in `client.rs:53`. Real traffic is
         * nothing like this -- measured against a live session, frames run a
         * median of 1.6 KiB -- so this is hundreds of ordinary packets of
         * headroom while still capping what a misbehaving server can make
         * this client hold.
         */
        const val PACKET_QUEUE_KIB = 8 * 1024

        /** How long a must-arrive packet waits for budget before rechecking [closed]. */
        const val BUDGET_WAIT_MS = 250L

        /**
         * A generous ceiling on `stream_config`: a real SPS+PPS pair is a few
         * hundred bytes. Bounding it separately matters because this is the
         * one kind that must never be dropped, so an unbounded one lets a
         * server hold the queue's whole budget with well-formed packets while
         * forcing an `AnnexB` scan and a codec rebuild for each.
         */
        const val MAX_STREAM_CONFIG_BYTES = 64 * 1024
    }
}
