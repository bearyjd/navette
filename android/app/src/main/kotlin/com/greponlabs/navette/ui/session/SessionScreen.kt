package com.greponlabs.navette.ui.session

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.Pairing
import com.greponlabs.navette.net.mediaWebSocketUrl
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.first


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
    pairing: Pairing,
    onLeave: () -> Unit,
) {
    // A reconnect is a clean rebuild: bumping the nonce recreates the
    // controller (and, below, the SurfaceView it drives) from scratch, so
    // every attempt reuses the exact open()/close() lifecycle a first attach
    // does rather than teaching the controller to re-open a torn-down socket.
    // The counters survive the rebuild -- they are keyed on the session, not
    // the nonce -- so the retry budget is spent across attempts, not reset by
    // each one.
    var reconnectNonce by remember(pairing.host, sessionName) { mutableIntStateOf(0) }
    var reconnectAttempt by remember(pairing.host, sessionName) { mutableIntStateOf(0) }
    // Zoom and pan outlive a rebuild too: a retry the user never noticed must
    // not snap a zoomed, panned view back to the corner.
    val transformHolder = remember(pairing.host, sessionName) { ViewTransformHolder() }
    // Keyed on the session, not the nonce, like transformHolder above: a
    // reconnect rebuilds SessionController, but the bridge's one-shot echo
    // and last-remote state must survive it. A fresh bridge per reconnect
    // forgets what the daemon last pushed here, and the lifecycle observer
    // re-added below re-syncs to ON_RESUME immediately on every rebuild --
    // without this, that resume would re-forward the daemon's own stale
    // push as if the phone had copied it new, clobbering whatever the guest
    // copied while the socket was down.
    val clipboardBridge = remember(pairing.host, sessionName) { ClipboardBridge() }
    val controller =
        remember(pairing.host, pairing.port, sessionName, reconnectNonce) {
            SessionController(
                mediaWebSocketUrl(pairing.host, sessionName, pairing.port),
                pairing.token,
                transformHolder,
                clipboardBridge,
            )
        }
    val state by controller.state.collectAsState()
    val focusRequester = remember { FocusRequester() }
    // Not owned by ImeLayer: the reconnect-safe effect below needs to
    // re-assert this focus target itself, not merely decline to steal it --
    // see that effect's comment for why "decline" alone was not enough.
    val fieldFocus = remember { FocusRequester() }
    val keyboard = LocalSoftwareKeyboardController.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val context = LocalContext.current
    val clipboard =
        remember(context) { context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager }
    // Keyed on the session, not the nonce, like the retry counters above: a
    // reconnect must not silently drop the user out of the on-screen
    // keyboard they had raised.
    var imeRaised by remember(pairing.host, sessionName) { mutableStateOf(false) }
    // Keyed like imeRaised, not the nonce: a reconnect rebuild must not
    // silently turn the HUD off while someone is watching it.
    var hudVisible by remember(pairing.host, sessionName) { mutableStateOf(false) }

    LockLandscapeWhileAttached()

    // Clipboard listener and lifecycle observer registered and torn down here,
    // in the same effect that opens and closes the controller -- so a leaked
    // OnPrimaryClipChangedListener cannot outlive the session. The ON_RESUME
    // read is not a fallback for the listener: since Android 10
    // getPrimaryClip() returns null when the app is not focused, and copying
    // from another app necessarily unfocuses Navette, so the listener alone
    // would miss the case this feature exists for.
    DisposableEffect(controller) {
        fun readLocalClipboard(): String? = clipboard.primaryClip?.getItemAt(0)?.coerceToText(context)?.toString()
        val clipboardListener =
            ClipboardManager.OnPrimaryClipChangedListener {
                readLocalClipboard()?.let { controller.onLocalClipboard(it) }
            }
        clipboard.addPrimaryClipChangedListener(clipboardListener)
        // Goes through onLocalClipboardResume, not onLocalClipboard: this
        // observer is re-added on every reconnect and syncs to the current
        // RESUMED state immediately, so it can fire on a resume that is
        // really "the socket just reopened", not just "the user switched
        // apps and back" -- see clipboardBridge's comment above for why that
        // case needs its own suppression.
        val clipboardObserver =
            LifecycleEventObserver { _, event ->
                if (event == Lifecycle.Event.ON_RESUME) {
                    readLocalClipboard()?.let { controller.onLocalClipboardResume(it) }
                }
            }
        lifecycleOwner.lifecycle.addObserver(clipboardObserver)
        controller.open()
        onDispose {
            clipboard.removePrimaryClipChangedListener(clipboardListener)
            lifecycleOwner.lifecycle.removeObserver(clipboardObserver)
            controller.close()
        }
    }

    LaunchedEffect(controller) {
        controller.onToggleHud = { hudVisible = !hudVisible }
        controller.onClipboardPush = { text -> clipboard.setPrimaryClip(ClipData.newPlainText("navette", text)) }
    }

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
            if (
                ReconnectPolicy.shouldRetry(
                    reconnectAttempt,
                    dropped.streamEnded,
                    dropped.decodeError,
                    dropped.connection is ConnectionState.Unauthorized,
                )
            ) {
                reconnectAttempt += 1
                delay(ReconnectPolicy.delayMs(reconnectAttempt))
                reconnectNonce += 1
            }
        }
    }

    val reconnecting =
        ReconnectPolicy.isDropped(state.connection) &&
            ReconnectPolicy.shouldRetry(
                reconnectAttempt,
                state.streamEnded,
                state.decodeError,
                state.connection is ConnectionState.Unauthorized,
            )
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
