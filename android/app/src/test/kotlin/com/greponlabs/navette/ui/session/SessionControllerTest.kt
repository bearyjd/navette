package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.BlobDescriptor
import com.greponlabs.navette.net.MediaInput
import com.greponlabs.navette.net.MediaPacket
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.test.setMain
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

private class FakeMediaSessionClient : MediaSessionClient {
    private val mutableConnection = MutableStateFlow<ConnectionState>(ConnectionState.Disconnected)
    override val connectionState: StateFlow<ConnectionState> = mutableConnection
    override var onPong: ((ULong) -> Unit)? = null
    override var onClipboard: ((String) -> Unit)? = null
    override var onClipboardBlob: ((BlobDescriptor) -> Unit)? = null
    var connectCalls = 0
    var closeCalls = 0
    var stateAfterConnect: ConnectionState = ConnectionState.Connected
    var acceptClipboard = true
    val sentClipboard = mutableListOf<String>()
    val sentInputs = mutableListOf<MediaInput>()

    override fun connect() {
        connectCalls += 1
        mutableConnection.value = stateAfterConnect
    }

    override fun close() {
        closeCalls += 1
        mutableConnection.value = ConnectionState.Disconnected
    }

    override suspend fun nextPacket(): MediaPacket? = null
    override fun sendInput(input: MediaInput): Boolean {
        sentInputs += input
        if (input is MediaInput.SetClipboard) {
            if (!acceptClipboard) return false
            sentClipboard += input.text
        }
        return true
    }
    override fun requestKeyframe() = Unit
    override fun sendPing(nonce: ULong): Boolean = true

    fun transitionTo(connection: ConnectionState) {
        mutableConnection.value = connection
    }
}

@OptIn(ExperimentalCoroutinesApi::class)
class SessionControllerTest {
    @Test
    fun `a fake media client drives controller connection state without platform media`() = runTest {
        Dispatchers.setMain(StandardTestDispatcher(testScheduler))
        try {
            val client = FakeMediaSessionClient()
            val controller =
                SessionController(
                    mediaUrl = "ws://unused/v1/sessions/demo/media",
                    token = "unused",
                    transformHolder = ViewTransformHolder(),
                    bridge = ClipboardBridge(),
                    client = client,
                )

            controller.open()
            runCurrent()

            assertEquals(1, client.connectCalls)
            assertTrue(controller.state.value.connection is ConnectionState.Connected)

            controller.close()
            assertEquals(1, client.closeCalls)
        } finally {
            Dispatchers.resetMain()
        }
    }

    @Test
    fun `a newer immediate clipboard send cancels an older parked retry`() = runTest {
        Dispatchers.setMain(StandardTestDispatcher(testScheduler))
        try {
            val client =
                FakeMediaSessionClient().apply {
                    stateAfterConnect = ConnectionState.Connecting
                    acceptClipboard = false
                }
            val controller =
                SessionController(
                    mediaUrl = "ws://unused/v1/sessions/demo/media",
                    token = "unused",
                    transformHolder = ViewTransformHolder(),
                    bridge = ClipboardBridge(),
                    client = client,
                )

            controller.open()
            runCurrent()
            controller.onLocalClipboard("A")
            runCurrent()

            client.acceptClipboard = true
            controller.onLocalClipboard("B")
            assertEquals(listOf("B"), client.sentClipboard)

            client.transitionTo(ConnectionState.Connected)
            runCurrent()
            assertEquals("the parked A retry must not overwrite B", listOf("B"), client.sentClipboard)

            controller.close()
        } finally {
            Dispatchers.resetMain()
        }
    }

    @Test
    fun localTextInvalidatesAnOlderImageUpload() = runTest {
        Dispatchers.setMain(StandardTestDispatcher(testScheduler))
        try {
            val upload = CompletableDeferred<BlobDescriptor?>()
            val transport =
                object : BlobTransport {
                    override suspend fun upload(mime: String, bytes: ByteArray): BlobDescriptor? = upload.await()

                    override suspend fun download(blob: BlobDescriptor): ByteArray? = null
                }
            val client = FakeMediaSessionClient()
            val controller =
                SessionController(
                    mediaUrl = "ws://unused/v1/sessions/demo/media",
                    token = "unused",
                    transformHolder = ViewTransformHolder(),
                    bridge = ClipboardBridge(),
                    client = client,
                    blobTransport = transport,
                )
            controller.open()
            controller.onLocalClipboardBlob("image/png", byteArrayOf(1))
            runCurrent()
            controller.onLocalClipboard("newer text")
            upload.complete(BlobDescriptor("0123456789abcdef0123456789abcdef", "image/png", 1))
            runCurrent()

            assertTrue(client.sentClipboard.contains("newer text"))
            assertTrue(
                "the completed old image must not reach the media socket",
                client.sentInputs.none { it is MediaInput.SetClipboardBlob },
            )
            controller.close()
        } finally {
            Dispatchers.resetMain()
        }
    }

    @Test
    fun remoteTextInvalidatesAnOlderImageDownload() = runTest {
        Dispatchers.setMain(StandardTestDispatcher(testScheduler))
        try {
            val download = CompletableDeferred<ByteArray?>()
            val transport =
                object : BlobTransport {
                    override suspend fun upload(mime: String, bytes: ByteArray): BlobDescriptor? = null

                    override suspend fun download(blob: BlobDescriptor): ByteArray? = download.await()
                }
            val client = FakeMediaSessionClient()
            val controller =
                SessionController(
                    mediaUrl = "ws://unused/v1/sessions/demo/media",
                    token = "unused",
                    transformHolder = ViewTransformHolder(),
                    bridge = ClipboardBridge(),
                    client = client,
                    blobTransport = transport,
                )
            val received = mutableListOf<ByteArray>()
            controller.onClipboardBlobPush = { _, bytes -> received += bytes }
            controller.open()
            client.onClipboardBlob?.invoke(BlobDescriptor("0123456789abcdef0123456789abcdef", "image/png", 1))
            runCurrent()
            client.onClipboard?.invoke("newer text")
            download.complete(byteArrayOf(1))
            runCurrent()

            assertTrue("the completed old image must not overwrite newer remote text", received.isEmpty())
            controller.close()
        } finally {
            Dispatchers.resetMain()
        }
    }
}
