package com.greponlabs.navette.net

import java.util.concurrent.CountDownLatch
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import okio.ByteString
import okio.ByteString.Companion.toByteString
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

/** Hand-written recording listener, per this project's no-mocking-framework convention. */
private class RecordingServer : WebSocketListener() {
    val opened = CountDownLatch(1)
    val closed = CountDownLatch(1)
    val textFrames = LinkedBlockingQueue<String>()

    @Volatile
    var socket: WebSocket? = null

    override fun onOpen(webSocket: WebSocket, response: Response) {
        socket = webSocket
        opened.countDown()
    }

    override fun onMessage(webSocket: WebSocket, text: String) {
        textFrames.add(text)
    }

    override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
        webSocket.close(code, reason)
    }

    override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
        closed.countDown()
    }

    override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
        closed.countDown()
    }

    fun awaitOpen() {
        assertTrue("the client never completed the WebSocket handshake", opened.await(5, TimeUnit.SECONDS))
    }

    fun nextTextFrame(): String? = textFrames.poll(5, TimeUnit.SECONDS)
}

class MediaClientTest {
    private lateinit var server: MockWebServer
    private lateinit var serverListener: RecordingServer
    private lateinit var client: MediaClient

    @Before
    fun setUp() {
        server = MockWebServer()
        server.start()
        serverListener = RecordingServer()
        server.enqueue(MockResponse().withWebSocketUpgrade(serverListener))
        val url = server.url("/v1/sessions/work/media").toString().replaceFirst("http://", "ws://")
        client = MediaClient(url, token = "test-token")
    }

    @After
    fun tearDown() {
        client.close()
        // MockWebServer's shutdown throws "Gave up waiting for queue to shut
        // down" if a WebSocket is still live, so the close handshake is given
        // a chance to land first. The shutdown itself is still guarded: this
        // is harness teardown, and letting it fail a test that already
        // asserted its behaviour reports the wrong thing.
        serverListener.closed.await(2, TimeUnit.SECONDS)
        runCatching { server.shutdown() }
    }

    /**
     * The server's `onOpen` fires on its own thread and the client's on
     * another, so waiting on the server side alone is not enough to observe
     * the client's own state transition.
     */
    private fun awaitClientConnected() {
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5)
        while (System.nanoTime() < deadline) {
            if (client.connectionState.value == ConnectionState.Connected) return
            Thread.sleep(10)
        }
        throw AssertionError("client never reached Connected, stuck at ${client.connectionState.value}")
    }

    private fun streamConfigPacket(streamId: Long, sequence: Long, codecConfig: ByteArray): MediaPacket =
        MediaPacket.of(
            MediaHeader(
                kind = MediaKind.STREAM_CONFIG,
                flags = MediaFlags.of(keyframe = false, discontinuity = false),
                streamId = streamId,
                sequence = sequence,
                timestampUs = sequence * 1000,
                payloadLen = 0,
                width = 1280,
                height = 720,
            ),
            StreamConfig(clientId = 11, surfaceId = 12, codecConfig = codecConfig).encode(),
        )

    private fun videoPacket(streamId: Long, sequence: Long, payload: ByteArray): MediaPacket =
        MediaPacket.of(
            MediaHeader(
                kind = MediaKind.VIDEO,
                flags = MediaFlags.of(keyframe = true, discontinuity = false),
                streamId = streamId,
                sequence = sequence,
                timestampUs = sequence * 1000,
                payloadLen = 0,
                width = 1280,
                height = 720,
            ),
            payload,
        )

    @Test
    fun `connect negotiates the media subprotocol`() {
        client.connect()
        serverListener.awaitOpen()

        val request = server.takeRequest(5, TimeUnit.SECONDS)
        assertEquals(MEDIA_WEBSOCKET_SUBPROTOCOL, request?.getHeader("Sec-WebSocket-Protocol"))
        assertEquals("/v1/sessions/work/media", request?.path)
        awaitClientConnected()
    }

    @Test
    fun `a keyframe request is the first frame sent after connecting`() {
        client.connect()
        serverListener.awaitOpen()

        assertEquals("""{"type":"request_keyframe"}""", serverListener.nextTextFrame())
    }

    @Test
    fun `binary frames decode into packets on the channel`() {
        client.connect()
        serverListener.awaitOpen()
        val packet = videoPacket(streamId = 7, sequence = 2, payload = byteArrayOf(0, 0, 0, 1, 0x65))

        serverListener.socket?.send(packet.encode().toByteString())

        val received = runBlocking { withTimeout(5_000) { client.nextPacket() } }
        assertEquals(packet, received)
        assertEquals(7L, received?.header?.streamId)
        assertEquals(2_000L, received?.header?.timestampUs)
    }

    /**
     * Mirrors `receive()`'s `Err(error) => ... true` in
     * `crates/navette-viewer/src/client.rs:318-321`: a packet this client
     * cannot parse is discarded, and the connection carries on. The valid
     * packet arriving afterwards is what proves the socket survived.
     */
    @Test
    fun `a malformed binary frame is dropped without closing the connection`() {
        client.connect()
        serverListener.awaitOpen()
        val good = videoPacket(streamId = 1, sequence = 5, payload = byteArrayOf(9))

        serverListener.socket?.send(ByteString.of(0x00, 0x01, 0x02))
        serverListener.socket?.send(ByteString.EMPTY)
        serverListener.socket?.send(good.encode().toByteString())

        val received = runBlocking { withTimeout(5_000) { client.nextPacket() } }
        assertEquals("the malformed frames must not have been queued", good, received)
        awaitClientConnected()
    }

    // The frame-size cap in MediaClient.onMessage cannot be exercised here:
    // MockWebServer's own outgoing queue tops out at 16 MiB, so it cannot
    // emit a frame above the cap to be refused. The guard is still what stands
    // between a hostile server and an out-of-memory kill, since OkHttp's
    // reader imposes no size limit of its own -- it is reviewed, not tested.
    // The budget arithmetic underneath it is unit-tested in PacketBudgetTest.

    /**
     * `stream_config` is the one kind that can never be dropped, so an
     * unbounded one lets a well-formed but abusive server hold the queue's
     * whole budget while forcing an AnnexB scan and a codec rebuild for each.
     * A real SPS+PPS is a few hundred bytes.
     */
    @Test
    fun `an implausibly large stream config is refused before it reaches the queue`() {
        client.connect()
        serverListener.awaitOpen()
        val good = videoPacket(streamId = 1, sequence = 9, payload = byteArrayOf(5))

        serverListener.socket?.send(
            streamConfigPacket(streamId = 1, sequence = 8, codecConfig = ByteArray(128 * 1024))
                .encode()
                .toByteString(),
        )
        serverListener.socket?.send(good.encode().toByteString())

        assertEquals("the oversized config must not have been queued", good, runBlocking { client.nextPacket() })
        awaitClientConnected()
    }

    @Test
    fun `a stream end carrying a payload is refused`() {
        client.connect()
        serverListener.awaitOpen()
        val good = videoPacket(streamId = 1, sequence = 11, payload = byteArrayOf(6))

        serverListener.socket?.send(
            MediaPacket.of(
                MediaHeader(
                    kind = MediaKind.STREAM_END,
                    flags = MediaFlags.of(keyframe = false, discontinuity = false),
                    streamId = 1,
                    sequence = 10,
                    timestampUs = 0,
                    payloadLen = 0,
                    width = 1280,
                    height = 720,
                ),
                ByteArray(64),
            ).encode().toByteString(),
        )
        serverListener.socket?.send(good.encode().toByteString())

        assertEquals(good, runBlocking { client.nextPacket() })
    }

    /**
     * A dropped access unit leaves the decoder missing a reference the frames
     * after it need, and nothing else produces a fresh keyframe before the
     * encoder's own GOP cadence. The request is also asked once, not once per
     * dropped packet -- the same property `client.rs` gets from its one-slot
     * keyframe channel.
     */
    @Test
    fun `overflowing the packet queue asks for a keyframe exactly once`() {
        client.connect()
        serverListener.awaitOpen()
        assertEquals("""{"type":"request_keyframe"}""", serverListener.nextTextFrame())

        // Nothing calls nextPacket(), so the queue fills and the rest drop.
        repeat(80) { sequence ->
            serverListener.socket?.send(
                videoPacket(streamId = 1, sequence = sequence.toLong(), payload = byteArrayOf(1, 2, 3))
                    .encode()
                    .toByteString(),
            )
        }

        assertEquals("""{"type":"request_keyframe"}""", serverListener.nextTextFrame())
        assertNull(
            "a sustained overflow must ask once, not once per dropped packet",
            serverListener.textFrames.poll(1, TimeUnit.SECONDS),
        )
    }

    @Test
    fun `a non-JSON text frame is tolerated`() {
        client.connect()
        serverListener.awaitOpen()
        val good = videoPacket(streamId = 1, sequence = 6, payload = byteArrayOf(4))

        serverListener.socket?.send("this is not JSON")
        serverListener.socket?.send(good.encode().toByteString())

        assertEquals(good, runBlocking { withTimeout(5_000) { client.nextPacket() } })
        awaitClientConnected()
    }

    @Test
    fun `out-of-range input is refused before it reaches the wire`() {
        client.connect()
        serverListener.awaitOpen()
        assertEquals("""{"type":"request_keyframe"}""", serverListener.nextTextFrame())

        assertTrue(client.sendInput(MediaInput.ViewportResize(1280, 720)))
        assertEquals("""{"type":"viewport_resize","width":1280,"height":720}""", serverListener.nextTextFrame())

        // Below MIN_VIEWPORT_WIDTH -- the bridge would reject it, so it never leaves.
        assertTrue(!client.sendInput(MediaInput.ViewportResize(10, 10)))
        assertNull("an invalid input must not reach the socket", serverListener.textFrames.poll(1, TimeUnit.SECONDS))
    }

    @Test
    fun `a server-side close ends the packet channel instead of hanging`() {
        client.connect()
        serverListener.awaitOpen()

        serverListener.socket?.close(1000, "session gone")

        runBlocking {
            withTimeout(5_000) {
                assertNull("the packet stream must end, not stall", client.nextPacket())
            }
        }
        assertEquals(ConnectionState.Disconnected, client.connectionState.value)
    }

    @Test
    fun `a malformed URL fails the connection instead of crashing the caller`() {
        val broken = MediaClient("ws://not a host:9417/v1/sessions/work/media", token = "test-token")
        broken.connect()
        assertTrue(broken.connectionState.value is ConnectionState.Failed)
    }

    /**
     * A rotated token otherwise produces an invisible infinite reconnect
     * loop: the phone spins forever while the daemon refuses every attempt,
     * read by the user as a network problem. This is the client-layer half
     * of that fix -- ReconnectPolicyTest pins the policy half.
     *
     * A second, throwaway server rather than the shared `server`/`client`
     * fixture: `setUp` already enqueues that server's one WebSocket-upgrade
     * response, and this test needs a plain 401 instead.
     */
    @Test
    fun `a 401 handshake becomes Unauthorized, not a generic failure`() =
        runBlocking {
            val unauthorizedServer = MockWebServer()
            unauthorizedServer.enqueue(MockResponse().setResponseCode(401))
            unauthorizedServer.start()
            val unauthorizedClient =
                MediaClient(
                    unauthorizedServer.url("/v1/sessions/work/media").toString().replace("http", "ws"),
                    token = "WRONG",
                )
            unauthorizedClient.connect()
            val state =
                withTimeout(5_000) {
                    unauthorizedClient.connectionState.first { it !is ConnectionState.Connecting }
                }
            assertEquals(ConnectionState.Unauthorized, state)
            unauthorizedServer.shutdown()
        }

    /**
     * The precise on-device failure behind the resume-clipboard bug:
     * `SessionScreen`'s `ON_RESUME` observer replays synchronously the
     * instant it is registered -- before `SessionController.open()`'s
     * `connect()` has run -- so the very first resume-triggered clipboard
     * send on every reattach used to call `sendInput` before this client
     * had ever connected. This is what that looks like at this layer: a
     * clean `false`, not a delayed delivery -- confirming there is nothing
     * here to retry against without the caller doing so itself, which is
     * exactly what `SessionController.sendClipboardOrRetryOnConnect` now
     * does (verified separately, on-device, three real reattaches). This
     * test cannot reach that private retry logic -- it pins the one-layer-down
     * behavior the fix depends on: a send before `connect()` is never
     * silently queued for later.
     */
    @Test
    fun `sendInput before connect is dropped, not silently queued for later`() {
        assertTrue(!client.sendInput(MediaInput.SetClipboard("never sent")))
    }
}
