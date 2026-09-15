package com.greponlabs.navette.ui.session

import android.view.KeyEvent
import android.view.MotionEvent
import android.view.Surface
import android.view.SurfaceHolder
import android.view.View
import com.greponlabs.navette.media.DecoderEvent
import com.greponlabs.navette.media.H264Decoder
import com.greponlabs.navette.media.PrimaryStream
import com.greponlabs.navette.media.StreamGate
import com.greponlabs.navette.media.StreamGateEvent
import com.greponlabs.navette.net.BTN_LEFT
import com.greponlabs.navette.net.BTN_RIGHT
import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.BlobDescriptor
import com.greponlabs.navette.net.MediaClient
import com.greponlabs.navette.net.MediaInput
import com.greponlabs.navette.net.MediaPacket
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch

/**
 * How long the surface's size must hold still before a resize is reported.
 *
 * Mirrors `RESIZE_DEBOUNCE` in `crates/navette-viewer/src/session.rs:23`.
 */
private const val RESIZE_DEBOUNCE_MS = 150L

/** How often the HUD pings and republishes. One second, matching [HUD_WINDOW_MS]. */
private const val HUD_SAMPLE_INTERVAL_MS: Long = 1000L

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

private class OkHttpMediaSessionClient(
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
 * Owns everything about this screen that outlives a recomposition: the media
 * socket, the stream gate, the decoder and the surface.
 *
 * Held in a `remember` so recomposition never rebuilds any of it -- tearing
 * down and recreating a `MediaCodec` per recomposition would be a black
 * flash per frame of UI state change.
 */
internal class SessionController(
    mediaUrl: String,
    token: String,
    private val transformHolder: ViewTransformHolder,
    // Passed in rather than owned, on the same terms as transformHolder:
    // remembered by the screen keyed on the session, not the reconnect
    // nonce, so a rebuilt controller does not start the bridge's one-shot
    // echo and last-remote state over from scratch. Reached from the same
    // two threads as `hud` -- the main thread via onLocalClipboard/
    // onLocalClipboardResume, OkHttp's reader thread via client.onClipboard
    // in open() -- so it is guarded by `lock` rather than getting its own.
    private val bridge: ClipboardBridge,
    private val client: MediaSessionClient = OkHttpMediaSessionClient(mediaUrl, token),
    private val blobTransport: BlobTransport = HttpBlobTransport(mediaUrl, token),
) {
    private val gate = StreamGate()
    private val scope = CoroutineScope(Dispatchers.Main.immediate + SupervisorJob())
    private val blobCoordinator =
        ClipboardBlobCoordinator(
            scope = scope,
            transport = blobTransport,
            announce = { blob -> client.sendInput(MediaInput.SetClipboardBlob(blob)) },
        )
    // The platform listener identifies the URI it observed. Only that exact
    // FileProvider URI may consume this one-shot echo, so an unrelated local
    // image cannot accidentally suppress the remote write's echo.
    private var localBlobEchoIdentity: String? = null

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

    // Bumped under `lock` by every startDecoder call. Window 2 refuses to
    // publish for a superseded claim, so the NEWEST call wins rather than
    // whichever happens to resume last -- see the check in startDecoder.
    private var startGeneration: Long = 0

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
     * Set by the screen, which owns the `ClipboardManager` call. Invoked on
     * [scope] (Main.immediate) from [client]'s `onClipboard`, which itself
     * runs on OkHttp's reader thread -- `ClipboardManager.setPrimaryClip`
     * must not be called from there.
     */
    var onClipboardPush: ((String) -> Unit)? = null

    /** Called with an image downloaded from the authenticated blob route. */
    var onClipboardBlobPush: ((BlobDescriptor, ByteArray, Long) -> Unit)? = null

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
        // Dispatched onto `scope` rather than invoked inline: this callback
        // runs on OkHttp's reader thread, and onClipboardPush ends in a
        // ClipboardManager call the screen must make from the main thread.
        client.onClipboard = { text ->
            // A text clipboard value is newer than any in-flight image blob
            // operation, even if the text bridge later identifies it as an
            // echo. Invalidate before making that decision so no old fetch
            // can overwrite the phone after this callback returns.
            blobCoordinator.invalidate()
            val toWrite = synchronized(lock) { bridge.onRemoteClipboard(text) }
            if (toWrite != null) scope.launch { onClipboardPush?.invoke(toWrite) }
        }
        client.onClipboardBlob = { blob ->
            blobCoordinator.download(blob) { bytes, claim ->
                onClipboardBlobPush?.invoke(blob, bytes, claim)
            }
        }
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
            scope.launchHudWhileConnected(client.connectionState) {
                while (true) {
                    val nonce = ++pingNonce
                    synchronized(lock) { hud.recordPing(nonce, System.currentTimeMillis()) }
                    client.sendPing(nonce)
                    // Sampling after the ping rather than before means the
                    // reading on screen is at most one interval behind the
                    // link, not two.
                    delay(HUD_SAMPLE_INTERVAL_MS)
                    // Read outside the lock: `gate.primary` is @Volatile, and
                    // the per-stream figures must describe the stream on the
                    // surface rather than whichever the socket last carried.
                    val rendered = gate.primary?.streamId
                    val sample = synchronized(lock) { hud.sample(System.currentTimeMillis(), rendered) }
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
        client.onClipboard = null
        client.onClipboardBlob = null
        blobCoordinator.invalidate()
        synchronized(lock) { localBlobEchoIdentity = null }
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
        // Claim a generation before touching anything. A later call claims a
        // higher one and both windows below stand down for the older claim,
        // so the newest call wins rather than whichever resumes last. Without
        // this, an older call resuming inside either window stops the newer
        // decoder -- via the capture below, or via `superseded` -- and leaves
        // the Surface showing a stale stream or nothing at all.
        val generation = synchronized(lock) { ++startGeneration }
        // Window 1: stop the outgoing decoder before publishing a successor.
        // If both happened in one window, surfaceDestroyed could capture the
        // new instance -- which holds nothing yet -- and return while the
        // old codec was still live on the Surface.
        val stopping =
            synchronized(lock) {
                // Stand down before touching `decoder` at all: a newer call
                // may already have published and started its decoder, and
                // capturing it here would stop it and leave the Surface with
                // nothing until the next reconfigure.
                if (generation != startGeneration) return
                decoder.also { decoder = null }
            }
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
        // HandlerThread. The generation check below should make that
        // unreachable -- every call that publishes held the highest claim, and
        // a lower claim returns before publishing -- but it is kept because a
        // future edit could break that invariant without anything noticing.
        val fresh =
            synchronized(lock) {
                // A newer startDecoder has claimed the decoder; stand down
                // rather than publish a stale stream over it.
                if (generation != startGeneration) return
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
            // Same guard, same reason: a superseded decoder draining its last
            // output buffers would otherwise put FPS/AGE/DEC readings on the
            // overlay for a stream that is no longer on the surface.
            is DecoderEvent.Presented -> {
                if (!isCurrent(source)) return
                synchronized(lock) { hud.recordPresented(event.timestampUs, System.currentTimeMillis()) }
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
                val panned = transform.panned(effect.dx.toDouble(), effect.dy.toDouble(), width, height)
                transform = panned.transform
                applyTransform()
                if (effect.handoffAtEdge && (panned.residualX != 0.0 || panned.residualY != 0.0)) {
                    sendScroll(panned.residualX.toFloat(), panned.residualY.toFloat())
                }
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
     * A local clipboard read from the listener firing. [bridge] decides
     * whether it is our own echo, unchanged, or genuinely new; only a
     * genuinely new value reaches [client].
     */
    fun onLocalClipboard(text: String) {
        blobCoordinator.invalidate()
        val forward = synchronized(lock) { bridge.onLocalClipboard(text) } ?: return
        sendClipboardOrRetryOnConnect(forward)
    }

    /** Starts an HTTP upload; only its completed descriptor reaches the socket. */
    fun onLocalClipboardBlob(mime: String, bytes: ByteArray, sourceIdentity: String? = null) {
        if (sourceIdentity != null && synchronized(lock) {
                if (localBlobEchoIdentity == sourceIdentity) {
                    localBlobEchoIdentity = null
                    true
                } else {
                    false
                }
            }
        ) return
        blobCoordinator.upload(mime, bytes)
    }

    /**
     * Commits a downloaded image to the Android clipboard only if no newer
     * clipboard event has invalidated its blob claim. The effect runs while
     * the coordinator's generation is serialized with invalidation.
     */
    fun commitClipboardBlob(claim: Long, identity: String, effect: () -> Unit) {
        blobCoordinator.commitIfCurrent(claim) {
            synchronized(lock) { localBlobEchoIdentity = identity }
            effect()
        }
    }

    /**
     * A local clipboard read from an `ON_RESUME`. Not routed through
     * [onLocalClipboard]: see [ClipboardBridge.onLocalClipboardResume] for
     * why a resume needs a suppression the listener path does not.
     */
    fun onLocalClipboardResume(text: String) {
        blobCoordinator.invalidate()
        val forward = synchronized(lock) { bridge.onLocalClipboardResume(text) } ?: return
        sendClipboardOrRetryOnConnect(forward)
    }

    /**
     * Tracks a clipboard send retried after [sendClipboardOrRetryOnConnect]
     * lost the race against the socket, so a second one can replace it
     * rather than the two eventually racing each other.
     */
    private var pendingClipboardResend: Job? = null

    /**
     * Sends now, or -- if [client] has no socket yet -- waits for the first
     * [ConnectionState.Connected] and sends then.
     *
     * Exists because `onLocalClipboardResume` is called from a
     * `LifecycleEventObserver` that is added to an already-resumed
     * lifecycle: per `androidx.lifecycle`, that add synchronously replays
     * the missed `ON_RESUME`, so the very first resume-triggered send on
     * every reattach happens before [open]'s `client.connect()` has run.
     * `MediaClient.sendInput` drops silently when that happens (logged
     * client-side as `"no socket"`) and nothing retried it -- confirmed on
     * a real device, 3/3 reconnects, before this existed. `client.connect()`
     * itself is likewise no guarantee: the handshake it starts is
     * asynchronous, so even a send issued after it returns is not
     * guaranteed to reach an open socket. Waiting on [ConnectionState]
     * rather than on any particular call having returned is what makes this
     * correct regardless of where in that async sequence the first attempt
     * landed.
     *
     * At most one retry is ever pending: every new decision cancels the old
     * retry *before* it attempts its own send. This ordering matters even
     * when the new send succeeds immediately: otherwise an older parked
     * retry can wake on the next connection and overwrite the newer text.
     * `pendingClipboardResend` is a child of `scope`, so `close()`'s
     * `scope.cancel()` already tears it down; no explicit cancel is needed
     * there.
     *
     * [ClipboardBridge.markSent] is called only where a send is actually
     * confirmed -- on both the immediate and the retried attempt -- never
     * merely because one was decided. A controller review caught the
     * earlier shape, where the decision methods wrote `lastSent`
     * themselves: a decision that lost the race here and then had its
     * retry cancelled too (a `close()` mid-wait) left `lastSent` already
     * holding the text, so the next resume of the same still-undelivered
     * clipboard saw a false match and silently gave up on it for the rest
     * of the session.
     */
    private fun sendClipboardOrRetryOnConnect(text: String) {
        pendingClipboardResend?.cancel()
        pendingClipboardResend = null
        if (client.sendInput(MediaInput.SetClipboard(text))) {
            synchronized(lock) { bridge.markSent(text) }
            return
        }
        pendingClipboardResend =
            scope.launch {
                client.connectionState.first { it is ConnectionState.Connected }
                if (client.sendInput(MediaInput.SetClipboard(text))) {
                    synchronized(lock) { bridge.markSent(text) }
                }
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
     * Reached for a two-finger drag at 1:1, or for the unconsumed part of a
     * zoomed pan at a content edge. Both are already screen-space pixel
     * deltas, so they need no rescaling before they become guest scroll
     * deltas. Gated like [sendButton]: an axis is delivered at the pointer's
     * position too.
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
