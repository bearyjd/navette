package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.BlobDescriptor
import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.MediaClient
import com.greponlabs.navette.net.MediaInput
import com.greponlabs.navette.net.MediaPacket
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.launch

/**
 * The media-socket contract the session lifecycle needs.
 *
 * Keeping this boundary here lets the controller be driven by a JVM fake
 * without constructing an OkHttp socket, MediaCodec, or Surface.
 */
internal interface MediaSessionClient {
    val connectionState: StateFlow<ConnectionState>
    var onPong: ((ULong) -> Unit)?
    var onClipboard: ((String) -> Unit)?
    var onClipboardBlob: ((BlobDescriptor) -> Unit)?

    fun connect()
    fun close()
    suspend fun nextPacket(): MediaPacket?
    fun sendInput(input: MediaInput): Boolean
    fun requestKeyframe()
    fun sendPing(nonce: ULong): Boolean
}

internal class OkHttpMediaSessionClient(
    mediaUrl: String,
    token: String,
) : MediaSessionClient {
    private val delegate = MediaClient(mediaUrl, token)

    override val connectionState: StateFlow<ConnectionState>
        get() = delegate.connectionState
    override var onPong: ((ULong) -> Unit)?
        get() = delegate.onPong
        set(value) {
            delegate.onPong = value
        }
    override var onClipboard: ((String) -> Unit)?
        get() = delegate.onClipboard
        set(value) {
            delegate.onClipboard = value
        }
    override var onClipboardBlob: ((BlobDescriptor) -> Unit)?
        get() = delegate.onClipboardBlob
        set(value) {
            delegate.onClipboardBlob = value
        }

    override fun connect() = delegate.connect()
    override fun close() = delegate.close()
    override suspend fun nextPacket(): MediaPacket? = delegate.nextPacket()
    override fun sendInput(input: MediaInput): Boolean = delegate.sendInput(input)
    override fun requestKeyframe() = delegate.requestKeyframe()
    override fun sendPing(nonce: ULong): Boolean = delegate.sendPing(nonce)
}

/**
 * Runs [work] only while the media socket is live.
 *
 * A controller survives its final failed reconnect so the screen can offer a
 * manual retry. Its HUD worker must not survive that socket: it would keep
 * waking once per second to ping a dead WebSocket and republish a changing
 * frame-age sample beneath the terminal overlay. `collectLatest` is
 * essential: [work] is an intentionally long-running loop, so an ordinary
 * `collect` would never observe the later failed/disconnected state.
 */
internal fun CoroutineScope.launchHudWhileConnected(
    connection: StateFlow<ConnectionState>,
    work: suspend () -> Unit,
): Job =
    launch {
        connection.collectLatest { state ->
            if (state is ConnectionState.Connected) work()
        }
    }
