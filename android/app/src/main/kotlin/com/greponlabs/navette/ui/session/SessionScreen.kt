package com.greponlabs.navette.ui.session

import android.view.KeyEvent
import android.view.MotionEvent
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import androidx.compose.foundation.background
import androidx.compose.foundation.focusable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableIntStateOf
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
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import com.greponlabs.navette.media.DecoderEvent
import com.greponlabs.navette.media.H264Decoder
import com.greponlabs.navette.media.PrimaryStream
import com.greponlabs.navette.media.StreamGate
import com.greponlabs.navette.media.StreamGateEvent
import com.greponlabs.navette.net.BTN_LEFT
import com.greponlabs.navette.net.BTN_RIGHT
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
import kotlinx.coroutines.flow.first
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

/** How often the HUD pings and republishes. One second, matching [HUD_WINDOW_MS]. */
private const val HUD_SAMPLE_INTERVAL_MS: Long = 1000L

/** What the screen renders. */
internal data class SessionUiState(
    // Connecting, not Disconnected: the controller opens the socket from a
    // DisposableEffect, which runs after the first composition, so a
    // Disconnected default would flash a "Disconnected" overlay on entry
    // every time.
    val connection: ConnectionState = ConnectionState.Connecting,
    val streamEnded: Boolean = false,
    val decodeError: String? = null,
    /** The size of the frame currently on the surface, or `null` before the first one. */
    val contentSize: Pair<Int, Int>? = null,
    /** The latest metrics sample, or `null` before the first one. */
    val hud: HudSample? = null,
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
    // A reconnect is a clean rebuild: bumping the nonce recreates the
    // controller (and, below, the SurfaceView it drives) from scratch, so
    // every attempt reuses the exact open()/close() lifecycle a first attach
    // does rather than teaching the controller to re-open a torn-down socket.
    // The counters survive the rebuild -- they are keyed on the session, not
    // the nonce -- so the retry budget is spent across attempts, not reset by
    // each one.
    var reconnectNonce by remember(host, sessionName) { mutableIntStateOf(0) }
    var reconnectAttempt by remember(host, sessionName) { mutableIntStateOf(0) }
    // Zoom and pan outlive a rebuild too: a retry the user never noticed must
    // not snap a zoomed, panned view back to the corner.
    val transformHolder = remember(host, sessionName) { ViewTransformHolder() }
    val controller =
        remember(host, sessionName, reconnectNonce) {
            SessionController(mediaWebSocketUrl(host, sessionName), transformHolder)
        }
    val state by controller.state.collectAsState()
    val focusRequester = remember { FocusRequester() }
    // Not owned by ImeLayer: the reconnect-safe effect below needs to
    // re-assert this focus target itself, not merely decline to steal it --
    // see that effect's comment for why "decline" alone was not enough.
    val fieldFocus = remember { FocusRequester() }
    val keyboard = LocalSoftwareKeyboardController.current
    val lifecycleOwner = LocalLifecycleOwner.current
    // Keyed on the session, not the nonce, like the retry counters above: a
    // reconnect must not silently drop the user out of the on-screen
    // keyboard they had raised.
    var imeRaised by remember(host, sessionName) { mutableStateOf(false) }
    // Keyed like imeRaised, not the nonce: a reconnect rebuild must not
    // silently turn the HUD off while someone is watching it.
    var hudVisible by remember(host, sessionName) { mutableStateOf(false) }

    LockLandscapeWhileAttached()

    DisposableEffect(controller) {
        controller.open()
        onDispose { controller.close() }
    }

    LaunchedEffect(controller) { controller.onToggleHud = { hudVisible = !hudVisible } }

    // Re-asserts whichever focus target is correct, every time this effect
    // reruns (keyed on the controller, so every reconnect). Merely skipping
    // the surface-focus request while the IME is raised was tried first and
    // measured wrong on a real device: the AndroidView holding the SurfaceView
    // is recreated on reconnect (`key(reconnectNonce)` below), and the
    // platform's own focus-search assigns that freshly attached View native
    // focus regardless of what Compose's FocusRequester state says -- the
    // hidden IME field lost real input focus even though `imeRaised` (and the
    // "Hide keyboard" label) correctly stayed true. Confirmed via
    // `dumpsys uiautomator`: the focused node was the surface's Box, and
    // adb-injected text was landing in the guest as hardware keys instead of
    // the IME delta path. Actively requesting the field's focus (and
    // re-showing the keyboard, since a view regaining focus does not raise it
    // on its own) closes that gap instead of merely not making it worse.
    LaunchedEffect(controller) {
        if (imeRaised) {
            fieldFocus.requestFocus()
            keyboard?.show()
        } else {
            focusRequester.requestFocus()
        }
    }

    // The retry budget refills only once the stream is genuinely live -- a
    // frame decoded -- not on the socket opening. A server that accepts the
    // socket and closes it straight away (session gone, upgrade refused
    // downstream) would otherwise satisfy an open-based reset on every
    // attempt and be retried forever.
    LaunchedEffect(controller) {
        controller.state.first { it.contentSize != null }
        reconnectAttempt = 0
    }

    // ...and whenever the screen comes back into the foreground. Retries spent
    // while it was hidden -- behind the keyguard a fold raises, with the app's
    // network restricted -- must not count against what happens once the user
    // can see it again. This is what turns "fold, swipe, dead Disconnected
    // screen" into "fold, swipe, picture back".
    LaunchedEffect(lifecycleOwner) {
        lifecycleOwner.repeatOnLifecycle(Lifecycle.State.STARTED) { reconnectAttempt = 0 }
    }

    // The retry itself. Two things about its shape are load-bearing. It reads
    // the controller's own flow rather than the recomposed `state` snapshot:
    // collectAsState keeps the dead controller's last value for a frame after
    // a swap, long enough for a snapshot-keyed effect to see "still dropped"
    // against the new controller and charge a second attempt for one drop.
    // And it runs only while STARTED, so it pauses behind the keyguard instead
    // of burning the budget on a network it cannot reach, and starts fresh --
    // against whatever the controller's state is by then -- when the screen
    // is visible again. Keyed on the controller so each rebuilt one waits for
    // its own drop.
    LaunchedEffect(controller, lifecycleOwner) {
        lifecycleOwner.repeatOnLifecycle(Lifecycle.State.STARTED) {
            val dropped =
                controller.state.first {
                    ReconnectPolicy.isDropped(it.connection) || it.streamEnded || it.decodeError != null
                }
            if (ReconnectPolicy.shouldRetry(reconnectAttempt, dropped.streamEnded, dropped.decodeError)) {
                reconnectAttempt += 1
                delay(ReconnectPolicy.delayMs(reconnectAttempt))
                reconnectNonce += 1
            }
        }
    }

    val reconnecting =
        ReconnectPolicy.isDropped(state.connection) &&
            ReconnectPolicy.shouldRetry(reconnectAttempt, state.streamEnded, state.decodeError)
    val onReconnect = {
        reconnectAttempt = 0
        reconnectNonce += 1
    }

    Box(
        modifier =
            Modifier
                .fillMaxSize()
                .background(Color.Black)
                .focusRequester(focusRequester)
                .focusable()
                .onKeyEvent { event -> controller.onKeyEvent(event.nativeKeyEvent) },
    ) {
        // Rebuilt whenever the reconnect nonce changes: a fresh SurfaceView
        // re-runs the factory below against the freshly-remembered controller,
        // which is what wires that controller's surface callback -- an
        // already-created holder never re-fires surfaceCreated for a callback
        // added later, so a controller swap without a new view would render
        // nothing.
        key(reconnectNonce) {
            AndroidView(
            // Touch is wired via View.setOnTouchListener on the raw
            // SurfaceView, not a Compose pointerInput modifier. This was
            // tried first because AndroidView always installs an internal
            // pointerInteropFilter that dispatches to the wrapped View
            // during Compose's Initial pointer-event pass, before ANY
            // pointerInput's Main-pass awaitFirstDown() runs -- a real,
            // documented mechanism by which a pointerInput on or around an
            // AndroidView can go silent. On this device, though, that
            // wasn't actually the fault: touch was reaching sendMotion/
            // sendButton correctly the whole time, under both this and the
            // original pointerInput-based approach: the real bug was
            // MediaInput's client_id/surface_id serializing as negative
            // Longs (see MediaProtocol.kt's header comment), which the
            // bridge silently rejected regardless of how input reached this
            // client. Kept anyway, now that it's verified working end to
            // end on a real device: it sidesteps the pointerInteropFilter
            // question entirely rather than merely ruling it out this once,
            // and needs no coroutine gesture-scope ceremony for
            // single-pointer tracking.
            factory = { context ->
                SurfaceView(context).apply {
                    holder.addCallback(controller.surfaceCallback)
                    setOnTouchListener { _, event -> controller.onTouchEvent(event) }
                    // Zoom and pan are a scale+translate on this view, applied
                    // by the controller synchronously from the touch path --
                    // not through Compose state and this AndroidView's
                    // `update` lambda. The framework inverse-maps every touch
                    // through the view's matrix before it reaches the listener
                    // (verified on device: a tap at screen x=2400 under a 2x
                    // scale arrived as x=1200), and the controller lifts it
                    // back into screen space using the transform it believes
                    // is applied. Those two must be the same matrix, and a
                    // transform that lands a frame later via recomposition
                    // would make every pinch step read the fingers through a
                    // stale one. Only the view's transform changes when
                    // zooming; its layout size never does, so no resize is
                    // reported and the guest window keeps its dimensions.
                    controller.bindView(this)
                }
            },
            modifier =
                Modifier
                    .fillMaxSize()
                    .onSizeChanged { size -> controller.onSurfaceResized(size.width, size.height) },
            )
        }

        ImeLayer(
            controller = controller,
            surfaceFocus = focusRequester,
            fieldFocus = fieldFocus,
            imeRaised = imeRaised,
            onImeRaisedChange = { imeRaised = it },
        )

        SessionHudOverlay(sample = state.hud, visible = hudVisible)

        SessionOverlay(
            state = state,
            reconnecting = reconnecting,
            reconnectAttempt = reconnectAttempt,
            maxAttempts = MAX_RECONNECT_ATTEMPTS,
            onReconnect = onReconnect,
            onLeave = onLeave,
        )
    }
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
 *
 * [imeRaised] and [fieldFocus] are hoisted to the caller rather than owned
 * here: a reconnect's own focus-restoring effect needs both -- whether the
 * IME is up, and the exact [FocusRequester] to point back at -- to actively
 * re-assert the field's focus after every rebuild, not merely decline to
 * steal it (see that effect's comment for why the weaker form measured wrong
 * on a real device).
 */
@Composable
private fun BoxScope.ImeLayer(
    controller: SessionController,
    surfaceFocus: FocusRequester,
    fieldFocus: FocusRequester,
    imeRaised: Boolean,
    onImeRaisedChange: (Boolean) -> Unit,
) {
    var typed by remember { mutableStateOf("") }
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
            val raised = !imeRaised
            onImeRaisedChange(raised)
            if (raised) {
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
private class SessionController(mediaUrl: String, private val transformHolder: ViewTransformHolder) {
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

    // Reached from four threads: the packet loop (Dispatchers.Default) via
    // recordPacket/recordFed, the codec's own callback thread via
    // recordPresented, hudJob (Main.immediate) via recordPing/sample, and
    // OkHttp's reader thread via onPong's recordPong. SessionHud is not
    // thread-safe, so every access takes `lock` -- the same lock `decoder`
    // uses, in the same order.
    private val hud = SessionHud()
    private var hudJob: Job? = null
    private var pingNonce: ULong = 0uL

    // Gesture state. All main-thread only: the touch listener, the surface
    // callbacks, and `scope` (Main.immediate) are the only writers. pressJob
    // must stay on that dispatcher -- moving it to Default would turn
    // leftPressed into a data race.
    private var view: View? = null
    private var transform: ViewTransform
        get() = transformHolder.value
        set(value) {
            transformHolder.value = value
        }
    private var gestureState: GestureState = GestureState.Idle
    private var pressJob: Job? = null
    private var leftPressed = false

    /** Set by the screen, which owns whether the HUD is showing. */
    var onToggleHud: (() -> Unit)? = null

    /**
     * The surface the most recent successfully-sent motion was addressed to.
     *
     * The bridge dispatches a button or axis at the pointer's *current*
     * position (`crates/navette-bridge/src/input.rs:119-145`), and only a
     * motion establishes pointer focus (`input.rs:81-90`). So a button that
     * goes out before any motion has reached its surface is a click at the
     * guest's top-left corner. That is reachable: a tap during the arming
     * window while the stream is still bootstrapping has its motion dropped
     * at `gate.primary == null`, and if the config lands inside those 60ms the
     * press would otherwise go out alone. Buttons and axes are gated on this
     * matching the stream they are about to be sent to.
     */
    private var motionSentTo: Long? = null

    private val _state = MutableStateFlow(SessionUiState())
    val state: StateFlow<SessionUiState> = _state.asStateFlow()

    /** The view zoom and pan are applied to; see the `AndroidView` factory for why it is held directly. */
    fun bindView(target: View) {
        view = target
        // The transform maths pivot at the top-left corner; the View default
        // is the centre, which would make a zoom drift away from the fingers.
        target.pivotX = 0f
        target.pivotY = 0f
        applyTransform()
    }

    private fun applyTransform() {
        val target = view ?: return
        target.scaleX = transform.zoom.toFloat()
        target.scaleY = transform.zoom.toFloat()
        target.translationX = transform.offsetX.toFloat()
        target.translationY = transform.offsetY.toFloat()
    }

    fun open() {
        connectionJob?.cancel()
        packetsJob?.cancel()
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
        // Installed before connect(), not after: connect() starts OkHttp's
        // reader thread, and that thread is what delivers pongs.
        client.onPong = { nonce -> synchronized(lock) { hud.recordPong(nonce, System.currentTimeMillis()) } }
        // connect() before the state collector, not after. The client's flow
        // starts at Disconnected; connect() moves it to Connecting
        // synchronously. Collecting first, on Main.immediate, would publish
        // that initial Disconnected into _state -- and the session screen's
        // retry waits on this flow for exactly that value, so the order here
        // is what keeps a fresh attach from being mistaken for a drop. Packets
        // that arrive before the collector runs sit in the client's queue.
        client.connect()
        connectionJob =
            scope.launch {
                client.connectionState.collect { connection -> _state.update { it.copy(connection = connection) } }
            }
        hudJob?.cancel()
        hudJob =
            scope.launch {
                while (true) {
                    val nonce = ++pingNonce
                    synchronized(lock) { hud.recordPing(nonce, System.currentTimeMillis()) }
                    client.sendPing(nonce)
                    // Sampling after the ping rather than before means the
                    // reading on screen is at most one interval behind the
                    // link, not two.
                    delay(HUD_SAMPLE_INTERVAL_MS)
                    val sample = synchronized(lock) { hud.sample(System.currentTimeMillis()) }
                    _state.update { it.copy(hud = sample) }
                }
            }
    }

    fun close() {
        // A screen left mid-drag must not strand a held button in the guest.
        pressJob?.cancel()
        if (leftPressed) sendButton(BTN_LEFT, pressed = false)
        leftPressed = false
        gestureState = GestureState.Idle
        view = null
        resizeJob?.cancel()
        packetsJob?.cancel()
        connectionJob?.cancel()
        hudJob?.cancel()
        client.onPong = null
        // Clear the surface BEFORE stopping the decoder. Cancelling
        // packetsJob above does not preempt a route() already inside
        // startDecoder, and with the old order that call could publish a
        // fresh decoder after stopDecoder had run -- leaking a codec and a
        // HandlerThread onto a Surface being torn down. With surface nulled
        // first, a racing publish bails at `surface ?: return`, and anything
        // published before the swap is still caught by stopDecoder below.
        synchronized(lock) { surface = null }
        stopDecoder(expected = null)
        client.close()
        scope.cancel()
    }

    private fun route(packet: MediaPacket) {
        synchronized(lock) { hud.recordPacket(System.currentTimeMillis(), packet) }
        when (val event = gate.handle(packet)) {
            is StreamGateEvent.Bootstrap -> startDecoder(event.stream)
            is StreamGateEvent.Reconfigure -> startDecoder(event.stream)
            is StreamGateEvent.Video -> {
                synchronized(lock) { hud.recordFed(event.timestampUs, System.currentTimeMillis()) }
                synchronized(lock) { decoder }?.feed(event.accessUnit, event.timestampUs)
            }
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
        // Window 1: stop the outgoing decoder before publishing a successor.
        // If both happened in one window, surfaceDestroyed could capture the
        // new instance -- which holds nothing yet -- and return while the
        // old codec was still live on the Surface.
        val stopping = synchronized(lock) { decoder.also { decoder = null } }
        // Outside the lock: stop() calls MediaCodec.release(), and the
        // codec's callback thread may be blocked on this very lock inside
        // recordPresented.
        stopping?.stop()

        // Captured by reference so the callback can name the instance it came
        // from. Without that, a failure raised by a decoder that has since
        // been replaced would tear down its successor instead. Nothing can
        // raise an event before the assignment below, because nothing has
        // called start() yet.
        var created: H264Decoder? = null
        var superseded: H264Decoder? = null
        // Window 2: publish. `superseded` catches anything a racing
        // startDecoder published in the gap between the two windows --
        // without it that instance would be overwritten unstopped,
        // unreachable through the field, and leak its codec and
        // HandlerThread.
        val fresh =
            synchronized(lock) {
                val target = surface ?: return
                val instance = H264Decoder(target) { event -> created?.let { onDecoderEvent(it, event) } }
                created = instance
                superseded = decoder
                decoder = instance
                instance
            }
        superseded?.stop()
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
        val stopping =
            synchronized(lock) {
                if (expected != null && decoder !== expected) return
                decoder.also { decoder = null }
            }
        // Outside the lock: stop() calls MediaCodec.release(), and the
        // codec's callback thread may be blocked on this very lock inside
        // recordPresented.
        stopping?.stop()
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
            is DecoderEvent.Presented ->
                synchronized(lock) { hud.recordPresented(event.timestampUs, System.currentTimeMillis()) }
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
             * The wait this incurs is `stop()`'s own `MediaCodec.release()`,
             * called outside [lock] as everywhere else in this file -- not
             * lock contention, since every section that holds [lock] is now a
             * brief field read or write. That is the accepted trade -- a
             * brief stall on leaving the screen beats rendering into a
             * released Surface -- and rapid session entry and exit is where
             * it would show, so it is on the on-device checklist.
             */
            override fun surfaceDestroyed(holder: SurfaceHolder) {
                val stopping =
                    synchronized(lock) {
                        surface = null
                        decoder.also { decoder = null }
                    }
                stopping?.stop()
            }
        }

    fun onSurfaceResized(width: Int, height: Int) {
        // A view can report 0x0 mid-layout, and during a reconnect's view
        // swap. Storing it would make sendMotion drop motions while buttons
        // kept firing; never store it, so the null-guards downstream stay
        // defence in depth rather than load-bearing.
        if (width <= 0 || height <= 0) return
        synchronized(lock) { surfaceSize = width to height }
        // A rotation or a fold must not leave the view panned off the content.
        transform = transform.clampedTo(width, height)
        applyTransform()
        resizeJob?.cancel()
        resizeJob =
            scope.launch {
                delay(RESIZE_DEBOUNCE_MS)
                val (clampedWidth, clampedHeight) = InputMapper.clampViewport(width, height) ?: return@launch
                client.sendInput(MediaInput.ViewportResize(clampedWidth, clampedHeight))
            }
    }

    /**
     * A raw `View.OnTouchListener` callback, not a Compose gesture -- see
     * the comment on the `AndroidView` call site for why. Reduces the
     * `MotionEvent` to a [TouchEvent], steps [GestureInterpreter], and
     * applies whatever it asks for. Returns `true` (event consumed) for every
     * action the interpreter knows, so the View system does not also try its
     * own default touch handling on it.
     *
     * Pointers are lifted into screen space first. The framework has already
     * inverse-mapped them through the view's transform, so while a pinch is
     * changing that transform, the local coordinates of a finger that has not
     * moved would change under it -- a feedback loop. Screen space is where
     * the fingers physically are, and stays put.
     */
    fun onTouchEvent(event: MotionEvent): Boolean {
        val action = touchAction(event.actionMasked) ?: return false
        val pointers =
            List(event.pointerCount) { index ->
                val (x, y) = transform.localToScreen(event.getX(index), event.getY(index))
                TouchPointer(event.getPointerId(index), x.toFloat(), y.toFloat())
            }
        val step =
            GestureInterpreter.step(
                gestureState,
                TouchEvent(
                    action = action,
                    actionPointerId = event.getPointerId(event.actionIndex),
                    pointers = pointers,
                    eventTimeMs = event.eventTime,
                    zoomed = transform.isZoomed,
                ),
            )
        gestureState = step.state
        for (effect in step.effects) applyEffect(effect)
        return true
    }

    private fun touchAction(actionMasked: Int): TouchAction? =
        when (actionMasked) {
            MotionEvent.ACTION_DOWN -> TouchAction.Down
            MotionEvent.ACTION_MOVE -> TouchAction.Move
            MotionEvent.ACTION_UP -> TouchAction.Up
            MotionEvent.ACTION_POINTER_DOWN -> TouchAction.PointerDown
            MotionEvent.ACTION_POINTER_UP -> TouchAction.PointerUp
            MotionEvent.ACTION_CANCEL -> TouchAction.Cancel
            else -> null
        }

    private fun applyEffect(effect: GestureEffect) {
        when (effect) {
            is GestureEffect.Motion -> {
                val (x, y) = transform.screenToLocal(effect.x, effect.y) ?: return
                sendMotion(x.toFloat(), y.toFloat())
            }
            GestureEffect.ArmLeftPress -> {
                pressJob?.cancel()
                pressJob =
                    scope.launch {
                        delay(PRESS_ARM_MS)
                        sendButton(BTN_LEFT, pressed = true)
                        leftPressed = true
                    }
            }
            GestureEffect.CancelLeftPress -> {
                pressJob?.cancel()
                if (leftPressed) sendButton(BTN_LEFT, pressed = false)
                leftPressed = false
            }
            // The fast-tap rule: a tap shorter than PRESS_ARM_MS reaches here
            // with its press never sent, and must still click.
            GestureEffect.EndLeftPress -> {
                pressJob?.cancel()
                if (!leftPressed) sendButton(BTN_LEFT, pressed = true)
                sendButton(BTN_LEFT, pressed = false)
                leftPressed = false
            }
            GestureEffect.RightClick -> {
                sendButton(BTN_RIGHT, pressed = true)
                sendButton(BTN_RIGHT, pressed = false)
            }
            is GestureEffect.Zoom -> {
                val (width, height) = synchronized(lock) { surfaceSize } ?: return
                transform =
                    transform.zoomedAbout(
                        effect.scaleFactor,
                        effect.focalX.toDouble(),
                        effect.focalY.toDouble(),
                        width,
                        height,
                    )
                applyTransform()
            }
            is GestureEffect.Pan -> {
                val (width, height) = synchronized(lock) { surfaceSize } ?: return
                transform = transform.pannedBy(effect.dx.toDouble(), effect.dy.toDouble(), width, height)
                applyTransform()
            }
            is GestureEffect.Scroll -> sendScroll(effect.dx, effect.dy)
            GestureEffect.ToggleHud -> onToggleHud?.invoke()
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
        if (client.sendInput(InputMapper.pointerMotion(stream.clientId, stream.surfaceId, mappedX, mappedY))) {
            motionSentTo = stream.surfaceId
        }
    }

    /** Refused, not repositioned, when no motion has reached [stream]: see [motionSentTo]. */
    private fun sendButton(button: Int, pressed: Boolean) {
        val stream = gate.primary ?: return
        if (motionSentTo != stream.surfaceId) return
        client.sendInput(InputMapper.pointerButton(stream.clientId, stream.surfaceId, button, pressed))
    }

    /**
     * Only ever reached at 1:1 (a two-finger drag while zoomed pans instead),
     * where screen and content pixels coincide, so the finger delta needs no
     * rescaling before it becomes a guest scroll delta. Gated like
     * [sendButton]: an axis is delivered at the pointer's position too.
     */
    private fun sendScroll(dx: Float, dy: Float) {
        val stream = gate.primary ?: return
        if (motionSentTo != stream.surfaceId) return
        client.sendInput(
            InputMapper.pointerAxis(
                stream.clientId,
                stream.surfaceId,
                InputMapper.scrollUnits(dx),
                InputMapper.scrollUnits(dy),
            ),
        )
    }
}

