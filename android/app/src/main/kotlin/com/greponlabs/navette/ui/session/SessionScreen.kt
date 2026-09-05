package com.greponlabs.navette.ui.session

import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
import android.content.pm.ActivityInfo
import android.view.KeyEvent
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import androidx.compose.foundation.background
import androidx.compose.foundation.focusable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.key.onKeyEvent
import androidx.compose.ui.input.pointer.PointerInputScope
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import com.greponlabs.navette.media.DecoderEvent
import com.greponlabs.navette.media.H264Decoder
import com.greponlabs.navette.media.PrimaryStream
import com.greponlabs.navette.media.StreamGate
import com.greponlabs.navette.media.StreamGateEvent
import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.MediaClient
import com.greponlabs.navette.net.MediaInput
import com.greponlabs.navette.net.MediaPacket
import com.greponlabs.navette.net.mediaWebSocketUrl
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch

/**
 * How long the surface's size must hold still before a resize is reported.
 *
 * Mirrors `RESIZE_DEBOUNCE` in `crates/navette-viewer/src/session.rs:23`. The
 * bridge debounces again at 100ms server-side; going slightly longer here
 * turns a drag into one message rather than a stream the server must absorb.
 */
private const val RESIZE_DEBOUNCE_MS = 150L

/** What the screen renders. */
private data class SessionUiState(
    // Connecting, not Disconnected: the controller opens the socket from a
    // DisposableEffect, which runs after the first composition, so a
    // Disconnected default would flash a "Disconnected" overlay on entry
    // every time.
    val connection: ConnectionState = ConnectionState.Connecting,
    val streamEnded: Boolean = false,
    val decodeError: String? = null,
    /** The size of the frame currently on the surface, or `null` before the first one. */
    val contentSize: Pair<Int, Int>? = null,
)

/**
 * The live session: a `SurfaceView` fed by `MediaCodec`, with touch, hardware
 * keyboard and on-screen IME forwarded back over the media socket.
 *
 * Locked to landscape while attached. `MediaInput.ViewportResize` is
 * server-validated to `320..3840` x `240..2160`
 * (`crates/navette-protocol/src/media.rs:307-311`) -- bounds shaped for a
 * desktop viewport, which a typical phone portrait size fails outright. The
 * surface reports its own pixel size as the viewport and the bridge
 * re-encodes at that size, exactly as the desktop viewer does, so there is no
 * letterbox and no aspect-fit mapping to do.
 *
 * State comes in and events go out, per this project's Compose convention --
 * but unlike `DrawerScreen` this screen necessarily owns live connection and
 * decoder state, which lives in the private [SessionController] below rather
 * than in `AppViewModel`. `AppViewModel` owns the control channel and which
 * session is active; it has never touched media state and does not start now.
 */
@Composable
fun SessionScreen(
    sessionName: String,
    host: String,
    onLeave: () -> Unit,
) {
    val controller =
        remember(host, sessionName) { SessionController(mediaWebSocketUrl(host, sessionName)) }
    val state by controller.state.collectAsState()
    val focusRequester = remember { FocusRequester() }

    LockLandscapeWhileAttached()

    DisposableEffect(controller) {
        controller.open()
        onDispose { controller.close() }
    }

    LaunchedEffect(controller) { focusRequester.requestFocus() }

    Box(
        modifier =
            Modifier
                .fillMaxSize()
                .background(Color.Black)
                .focusRequester(focusRequester)
                .focusable()
                .onKeyEvent { event -> controller.onKeyEvent(event.nativeKeyEvent) },
    ) {
        AndroidView(
            factory = { context -> SurfaceView(context).apply { holder.addCallback(controller.surfaceCallback) } },
            modifier =
                Modifier
                    .fillMaxSize()
                    .onSizeChanged { size -> controller.onSurfaceResized(size.width, size.height) }
                    .pointerInput(controller) { controller.trackPointer(this) },
        )

        ImeLayer(controller = controller, surfaceFocus = focusRequester)

        SessionOverlay(state = state, onLeave = onLeave)
    }
}

/**
 * Sets landscape on entry and unlocks orientation on exit.
 *
 * Restores `UNSPECIFIED` rather than whatever `requestedOrientation` held on
 * entry: nothing else in this app sets it, and reading it back to restore it
 * is precisely what breaks if the Activity is ever recreated mid-lock -- the
 * captured "previous" would be the lock this effect just applied, leaving the
 * whole app landscape-locked for the rest of the process. `MainActivity`
 * declares `configChanges` so that recreation does not happen; this makes the
 * restore correct even if it did.
 *
 * The Activity is found by walking the `ContextWrapper` chain rather than
 * casting `LocalContext.current`: today's host is `MainActivity` calling
 * `setContent` directly, but a hard cast that starts crashing if that ever
 * changes is the worst failure mode for a screen with no automated coverage.
 */
@Composable
private fun LockLandscapeWhileAttached() {
    val context = LocalContext.current
    DisposableEffect(context) {
        val activity = context.findActivity()
        activity?.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_LANDSCAPE
        onDispose { activity?.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED }
    }
}

private tailrec fun Context.findActivity(): Activity? =
    when (this) {
        is Activity -> this
        is ContextWrapper -> baseContext.findActivity()
        else -> null
    }

/**
 * The on-screen-keyboard path: an off-screen text field that reports what the
 * IME commits, plus the toggle that raises it.
 *
 * The field is sized 1.dp at zero alpha rather than truly zero-size, since a
 * zero-size composable can be skipped by the IME system on some versions.
 *
 * **The field is never cleared.** Clearing it would fire `onValueChange("")`,
 * which diffs as a deletion of everything typed so far and would send that
 * many backspaces to the guest -- destroying the very text the user just
 * committed. `typed` advances in lockstep with the field instead, so every
 * change is diffed against exactly what preceded it.
 *
 * The toggle exists because focus is the screen's scarce resource: while the
 * field holds it the soft keyboard is up and consuming keys, and while the
 * video surface holds it a hardware keyboard works. Handing focus back to
 * [surfaceFocus] on dismissal is what restores the hardware path.
 */
@Composable
private fun BoxScope.ImeLayer(controller: SessionController, surfaceFocus: FocusRequester) {
    var typed by remember { mutableStateOf("") }
    var imeRaised by remember { mutableStateOf(false) }
    val fieldFocus = remember { FocusRequester() }
    val keyboard = LocalSoftwareKeyboardController.current

    BasicTextField(
        value = typed,
        onValueChange = { next ->
            controller.onImeText(previous = typed, current = next)
            typed = next
        },
        modifier = Modifier.size(1.dp).alpha(0f).focusRequester(fieldFocus),
    )

    TextButton(
        onClick = {
            imeRaised = !imeRaised
            if (imeRaised) {
                fieldFocus.requestFocus()
                keyboard?.show()
            } else {
                keyboard?.hide()
                surfaceFocus.requestFocus()
            }
        },
        modifier = Modifier.align(Alignment.TopEnd).padding(8.dp),
    ) {
        Text(text = if (imeRaised) "Hide keyboard" else "Keyboard", color = Color.White)
    }
}

/**
 * Owns everything about this screen that outlives a recomposition: the media
 * socket, the stream gate, the decoder and the surface.
 *
 * Held in a `remember` so recomposition never rebuilds any of it -- tearing
 * down and recreating a `MediaCodec` per recomposition would be a black
 * flash per frame of UI state change.
 */
private class SessionController(mediaUrl: String) {
    private val client = MediaClient(mediaUrl)
    private val gate = StreamGate()
    private val scope = CoroutineScope(Dispatchers.Main.immediate + SupervisorJob())

    /**
     * Guards [decoder], [surface] and [surfaceSize], which two threads reach:
     * the surface callbacks on the main thread, and the packet loop on a
     * background dispatcher. Lock order is always this before
     * [H264Decoder]'s own -- never the reverse -- and no call out to Compose
     * or the socket happens while it is held.
     */
    private val lock = Any()

    private var decoder: H264Decoder? = null
    private var surface: Surface? = null
    private var surfaceSize: Pair<Int, Int>? = null

    // Tracked so each is cancelled before being replaced, matching
    // AppViewModel's connectionJob/refreshJob discipline: a StateFlow and a
    // Channel both outlive their producer, so a leaked collector here would
    // sit idle for the rest of the screen's life.
    private var connectionJob: Job? = null
    private var packetsJob: Job? = null
    private var resizeJob: Job? = null

    private val _state = MutableStateFlow(SessionUiState())
    val state: StateFlow<SessionUiState> = _state.asStateFlow()

    fun open() {
        connectionJob?.cancel()
        packetsJob?.cancel()
        connectionJob =
            scope.launch {
                client.connectionState.collect { connection -> _state.update { it.copy(connection = connection) } }
            }
        // Deliberately NOT on the main dispatcher. This loop calls
        // MediaCodec.configure()+start() on a stream bootstrap, which costs
        // tens to hundreds of milliseconds; on Main that is a visible stall,
        // and during it the socket's bounded packet queue fills, which is what
        // makes MediaClient park its reader thread long enough to risk a ping
        // timeout. _state is a StateFlow and collectAsState delivers on Main,
        // so UI updates from here need no hop of their own.
        packetsJob =
            scope.launch(Dispatchers.Default) {
                while (true) {
                    val packet = client.nextPacket() ?: break
                    route(packet)
                }
            }
        client.connect()
    }

    fun close() {
        resizeJob?.cancel()
        packetsJob?.cancel()
        connectionJob?.cancel()
        stopDecoder(expected = null)
        synchronized(lock) { surface = null }
        client.close()
        scope.cancel()
    }

    private fun route(packet: MediaPacket) {
        when (val event = gate.handle(packet)) {
            is StreamGateEvent.Bootstrap -> startDecoder(event.stream)
            is StreamGateEvent.Reconfigure -> startDecoder(event.stream)
            is StreamGateEvent.Video -> synchronized(lock) { decoder }?.feed(event.accessUnit, event.timestampUs)
            StreamGateEvent.Ended -> _state.update { it.copy(streamEnded = true) }
            null -> Unit
        }
    }

    /**
     * No-op until the surface exists. That is not a lost bootstrap: the gate
     * retains the adopted stream, and [surfaceCallback]'s `surfaceCreated`
     * picks it back up.
     */
    private fun startDecoder(stream: PrimaryStream) {
        // Captured by reference so the callback can name the instance it came
        // from. Without that, a failure raised by a decoder that has since
        // been replaced would tear down its successor instead. Nothing can
        // raise an event before the assignment below, because nothing has
        // called start() yet.
        var created: H264Decoder? = null
        val fresh =
            synchronized(lock) {
                val target = surface ?: return
                decoder?.stop()
                val instance = H264Decoder(target) { event -> created?.let { onDecoderEvent(it, event) } }
                created = instance
                decoder = instance
                instance
            }
        _state.update { it.copy(streamEnded = false, decodeError = null, contentSize = null) }
        // Started outside the lock. configure()+start() costs tens to hundreds
        // of milliseconds, and holding the lock across it would block
        // surfaceDestroyed on the main thread for that long. If the surface
        // does go away first, stop() marks this instance released and start()
        // becomes a no-op rather than configuring onto a dead Surface.
        fresh.start(stream)
    }

    /**
     * Stops [expected], or the current decoder when it is `null`.
     *
     * Naming the instance matters for the failure path: a teardown queued for
     * a decoder that errored must not stop whichever decoder happens to be
     * current by the time it runs. The triggering sequence -- a bad bitstream
     * followed immediately by the server's recovery `StreamConfig` -- is the
     * case this code exists for, not a corner.
     */
    private fun stopDecoder(expected: H264Decoder?) {
        synchronized(lock) {
            if (expected != null && decoder !== expected) return
            decoder?.stop()
            decoder = null
        }
    }

    /**
     * Raised from whichever thread produced the event -- the codec's own
     * `HandlerThread` for [DecoderEvent.Configured] and
     * [DecoderEvent.Failed], the packet loop for
     * [DecoderEvent.NeedsKeyframe] -- so state goes through a StateFlow
     * rather than a Compose write.
     */
    private fun onDecoderEvent(source: H264Decoder, event: DecoderEvent) {
        when (event) {
            // Ignored if it came from a decoder that has since been replaced:
            // a late frame size from a dead decoder would misreport what is on
            // the surface and skew touch mapping.
            is DecoderEvent.Configured -> {
                if (!isCurrent(source)) return
                _state.update { it.copy(contentSize = event.width to event.height, decodeError = null) }
            }
            // Through requestKeyframe(), not sendInput(), so the decoder's
            // drops share the client's once-until-one-arrives gate rather than
            // asking per dropped access unit.
            DecoderEvent.NeedsKeyframe -> client.requestKeyframe()
            is DecoderEvent.Failed -> {
                if (!isCurrent(source)) return
                _state.update { it.copy(decodeError = event.reason) }
                // A failed codec is left in Error state holding its own
                // HandlerThread, so it has to be torn down rather than merely
                // reported. Dispatched rather than called inline: this arrives
                // on the codec's callback thread, and releasing a MediaCodec
                // from inside its own MediaCodec.Callback can deadlock. The
                // teardown names `source`, so a replacement built in the
                // meantime survives.
                scope.launch(Dispatchers.Default) { stopDecoder(source) }
            }
        }
    }

    private fun isCurrent(candidate: H264Decoder): Boolean = synchronized(lock) { decoder === candidate }

    val surfaceCallback =
        object : SurfaceHolder.Callback {
            override fun surfaceCreated(holder: SurfaceHolder) {
                synchronized(lock) { surface = holder.surface }
                val stream = gate.primary ?: return
                startDecoder(stream)
                // Access units that arrived before the surface existed had
                // nowhere to go. Asking for a keyframe is the bridge's own
                // recovery path (see client.rs's decode-failure handling) and
                // is what re-primes the picture here.
                client.sendInput(MediaInput.RequestKeyframe)
            }

            override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
                onSurfaceResized(width, height)
            }

            /**
             * Must finish before returning: a Surface released under a running
             * codec throws.
             *
             * This can block on [lock] while the packet loop is starting a
             * codec, which costs tens to hundreds of milliseconds on the main
             * thread. That is the accepted trade -- a brief stall on leaving
             * the screen beats rendering into a released Surface -- and rapid
             * session entry and exit is where it would show, so it is on the
             * on-device checklist.
             */
            override fun surfaceDestroyed(holder: SurfaceHolder) {
                synchronized(lock) {
                    decoder?.stop()
                    decoder = null
                    surface = null
                }
            }
        }

    fun onSurfaceResized(width: Int, height: Int) {
        synchronized(lock) { surfaceSize = width to height }
        resizeJob?.cancel()
        resizeJob =
            scope.launch {
                delay(RESIZE_DEBOUNCE_MS)
                val (clampedWidth, clampedHeight) = InputMapper.clampViewport(width, height) ?: return@launch
                client.sendInput(MediaInput.ViewportResize(clampedWidth, clampedHeight))
            }
    }

    suspend fun trackPointer(pointerScope: PointerInputScope) =
        with(pointerScope) {
            awaitEachGesture {
                val down = awaitFirstDown(requireUnconsumed = false)
                down.consume()
                sendMotion(down.position.x, down.position.y)
                sendButton(pressed = true)
                while (true) {
                    val event = awaitPointerEvent()
                    val change = event.changes.firstOrNull { it.id == down.id }
                    if (change == null || !change.pressed) break
                    sendMotion(change.position.x, change.position.y)
                    change.consume()
                }
                sendButton(pressed = false)
            }
        }

    /** Returns whether the key was consumed; an unmapped one is left to the system. */
    fun onKeyEvent(event: KeyEvent): Boolean {
        val stream = gate.primary ?: return false
        val evdevCode = KeycodeMap.androidKeycodeToEvdev(event.keyCode) ?: return false
        when (event.action) {
            KeyEvent.ACTION_DOWN -> {
                // Auto-repeat is the guest's own responsibility, driven off a
                // single press. A repeated down is the duplicate
                // navette-bridge's InputState collapses to a no-op anyway
                // (crates/navette-bridge/src/input.rs:157-167) -- better not
                // to be the client that sends it.
                if (event.repeatCount > 0) return true
                client.sendInput(InputMapper.keyboardModifiers(stream.clientId, stream.surfaceId, event.metaState))
                client.sendInput(InputMapper.keyboardKey(stream.clientId, stream.surfaceId, evdevCode, true))
            }
            KeyEvent.ACTION_UP -> {
                client.sendInput(InputMapper.keyboardKey(stream.clientId, stream.surfaceId, evdevCode, false))
                client.sendInput(InputMapper.keyboardModifiers(stream.clientId, stream.surfaceId, event.metaState))
            }
            else -> return false
        }
        return true
    }

    fun onImeText(previous: String, current: String) {
        val stream = gate.primary ?: return
        for (input in InputMapper.imeTextDelta(stream.clientId, stream.surfaceId, previous, current)) {
            client.sendInput(input)
        }
    }

    /**
     * The surface identity is read fresh on every event rather than captured
     * once: a `Reconfigure` can hand the session a new `surface_id`, and the
     * bridge rejects input addressed to a stale one.
     */
    private fun sendMotion(x: Float, y: Float) {
        val stream = gate.primary ?: return
        val (surfaceWidth, surfaceHeight) = synchronized(lock) { surfaceSize } ?: return
        // Before the first frame the two spaces are identical by definition:
        // the surface's own size is what was reported as the viewport.
        val (contentWidth, contentHeight) = _state.value.contentSize ?: (surfaceWidth to surfaceHeight)
        val (mappedX, mappedY) =
            InputMapper.rescaleToContent(x, y, surfaceWidth, surfaceHeight, contentWidth, contentHeight) ?: return
        client.sendInput(InputMapper.pointerMotion(stream.clientId, stream.surfaceId, mappedX, mappedY))
    }

    private fun sendButton(pressed: Boolean) {
        val stream = gate.primary ?: return
        client.sendInput(InputMapper.pointerButton(stream.clientId, stream.surfaceId, pressed))
    }
}

/** Connection and error states drawn over the video. */
@Composable
private fun SessionOverlay(state: SessionUiState, onLeave: () -> Unit) {
    val message =
        when {
            state.decodeError != null -> state.decodeError
            state.streamEnded -> "The session's window closed."
            state.connection is ConnectionState.Failed -> "Disconnected: ${state.connection.reason}"
            state.connection is ConnectionState.Disconnected -> "Disconnected."
            state.connection is ConnectionState.Connecting -> null
            state.contentSize == null -> null
            else -> return
        }

    Box(modifier = Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
        Column(
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            if (message == null) {
                CircularProgressIndicator()
                Text(
                    text = "Waiting for the first frame...",
                    color = Color.White,
                    style = MaterialTheme.typography.bodyMedium,
                )
            } else {
                Text(
                    text = message,
                    color = Color.White,
                    style = MaterialTheme.typography.bodyLarge,
                    modifier = Modifier.padding(horizontal = 24.dp),
                )
                Button(onClick = onLeave) { Text("Back to sessions") }
            }
        }
    }
}
