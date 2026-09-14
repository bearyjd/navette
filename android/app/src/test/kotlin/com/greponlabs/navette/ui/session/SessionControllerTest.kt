package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.MediaInput
import com.greponlabs.navette.net.MediaPacket
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
    var connectCalls = 0
    var closeCalls = 0
    var stateAfterConnect: ConnectionState = ConnectionState.Connected
    var acceptClipboard = true
    val sentClipboard = mutableListOf<String>()

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
}
