package com.greponlabs.navette.ui.session

import android.view.KeyEvent
import com.greponlabs.navette.media.PrimaryStream
import com.greponlabs.navette.net.MediaInput
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * Everything keyboard-shaped that reaches the guest: hardware keys, the
 * on-screen IME's text diff, the key bar, and the sticky Ctrl/Alt that wrap
 * the latter two.
 *
 * Split out of [SessionController] so the send composition can be driven on
 * the JVM: the controller's `gate.primary` is only ever set by its packet
 * loop on `Dispatchers.Default`, outside any test scheduler, so a key path
 * living on the controller could not be exercised. Here the stream and the
 * send are lambdas the controller supplies (`{ gate.primary }` and
 * `client::sendInput`), and a test supplies its own.
 *
 * Main-thread only, like the controller's gesture state: the key listener,
 * the hidden IME field and the key bar are the only callers. The sticky
 * state resets with the controller on a reconnect rebuild -- see
 * [StickyModifiers] for why that is the acceptable surprise.
 */
internal class SessionKeyboard(
    private val primary: () -> PrimaryStream?,
    private val send: (MediaInput) -> Boolean,
) {
    private val _modifiers = MutableStateFlow(StickyModifiers())

    /** What the key bar renders: which chips are armed or locked. */
    val modifiers: StateFlow<StickyModifiers> = _modifiers.asStateFlow()

    /**
     * Tap to arm, tap again to disarm. Disarming also *releases* the key in
     * the guest, which is the manual recovery for a Ctrl the guest still
     * believes is down because a chord's release was dropped downstream: the
     * client cannot detect that (the send was accepted here), so tapping the
     * chip off is the user's way to ask for it again. Releases are safe to
     * repeat -- the bridge forwards a release for a keycode it never saw
     * pressed, collapsing only a redundant *press*
     * (`crates/navette-bridge/src/input.rs:159-198`).
     */
    fun onModifierTapped(key: StickyKey) {
        val next = _modifiers.value.tapped(key)
        if (_modifiers.value.holds(key) && !next.holds(key)) releaseKeys(listOf(key.evdevCode))
        publish(next)
    }

    fun onModifierLocked(key: StickyKey) = publish(_modifiers.value.locked(key))

    /**
     * Releases every modifier this bar can hold, whatever it believes is
     * down, and forgets the sticky state.
     *
     * Called when the session screen goes away: a modifier left pressed in
     * the guest outlives the client that pressed it. The server heals held
     * keys when an attachment drops, but that healing travels through the
     * same bounded input queue that may have dropped the release in the first
     * place, so the client says it explicitly rather than relying on it.
     */
    fun releaseHeldModifiers() {
        releaseKeys(listOf(KeycodeMap.KEY_LEFTCTRL, KeycodeMap.KEY_LEFTALT, KeycodeMap.KEY_LEFTSHIFT))
        // Published, though the usual caller is `close()`: the chips must not
        // claim to hold a modifier this just released. Safe from there -- a
        // StateFlow write, whose only collector is the screen being disposed
        // in the same pass (and on a reconnect the screen has already moved to
        // the replacement controller's keyboard).
        publish(StickyModifiers())
    }

    /**
     * A hardware key. Returns whether it was consumed; an unmapped one is
     * left to the system. The sticky modifiers do not apply here: a hardware
     * keyboard has a Ctrl of its own, reported through `metaState`.
     */
    fun onKeyEvent(event: KeyEvent): Boolean {
        val stream = primary() ?: return false
        val evdevCode = KeycodeMap.androidKeycodeToEvdev(event.keyCode) ?: return false
        when (event.action) {
            KeyEvent.ACTION_DOWN -> {
                // Auto-repeat is the guest's own responsibility, driven off a
                // single press. A repeated down is the duplicate
                // navette-bridge's InputState collapses to a no-op anyway
                // (crates/navette-bridge/src/input.rs:157-167) -- better not
                // to be the client that sends it.
                if (event.repeatCount > 0) return true
                // Stops at a refusal, like a chord: a key press whose modifier
                // state never arrived would reach the guest unmodified -- a
                // plain "c" where the user typed Ctrl+C.
                sendWhileAccepted(
                    listOf(
                        InputMapper.keyboardModifiers(stream.clientId, stream.surfaceId, event.metaState),
                        InputMapper.keyboardKey(stream.clientId, stream.surfaceId, evdevCode, true),
                    ),
                )
            }
            // Both attempted whatever the other does: each is a return to
            // neutral, and a release that does not go out leaves the key down
            // in the guest until the attachment drops. There is no unwind for
            // a refused release -- the release *is* the unwind -- so the most
            // this path can do is always try. `releaseHeldModifiers` on close
            // is the backstop for the modifier half.
            KeyEvent.ACTION_UP -> {
                send(InputMapper.keyboardKey(stream.clientId, stream.surfaceId, evdevCode, false))
                send(InputMapper.keyboardModifiers(stream.clientId, stream.surfaceId, event.metaState))
            }
            else -> return false
        }
        return true
    }

    /**
     * A key bar key, or the empty-field Backspace the hidden field forwards:
     * one chord under whatever is held, then the armed modifiers are spent.
     *
     * Nothing is spent unless something reached the guest -- silent before the
     * first `StreamConfig` like [onKeyEvent], and silent when the socket
     * refuses, so an armed Ctrl survives a chord that never went out. A chord
     * cut off partway is unwound: the modifier presses that *did* land would
     * otherwise stay down in the guest for every later key.
     */
    fun onKeyBarKey(evdevCode: Int, needsShift: Boolean = false) {
        val stream = primary() ?: return
        val held = _modifiers.value.held()
        val chord = InputMapper.keyChord(stream.clientId, stream.surfaceId, evdevCode, needsShift, held)
        val delivered = sendWhileAccepted(chord)
        if (delivered == 0) return
        if (delivered < chord.size) unwind(held, releaseShift = needsShift)
        publish(_modifiers.value.consumed())
    }

    /**
     * An on-screen-keyboard edit, as the field's previous and current text.
     * Returns whether a held modifier wrapped the typed text, so the screen
     * can drop that text from its diff base: Gboard committed "c" for
     * Ctrl+C, and the guest must not later see a backspace for a character
     * it never received as text. A pure deletion is never chorded and spends
     * nothing -- see [InputMapper.imeTextDelta].
     */
    fun onImeText(previous: String, current: String): Boolean {
        val stream = primary() ?: return false
        val held = _modifiers.value.held()
        val events = InputMapper.imeTextDelta(stream.clientId, stream.surfaceId, previous, current, held)
        val delivered = sendWhileAccepted(events)
        // releaseShift unconditionally here: a delta can hold several
        // chords and a shifted character anywhere in it, and a Shift left
        // down would mangle every later character rather than one.
        if (delivered < events.size) unwind(held, releaseShift = true)
        // `delivered > 0` for the same reason as [onKeyBarKey]: a modifier the
        // guest never saw must not be spent, and the field must keep the text
        // it is still the only record of.
        val chorded = held.any && delivered > 0 && InputMapper.typedChars(previous, current) > 0
        if (chorded) publish(_modifiers.value.consumed())
        return chorded
    }

    /**
     * Sends in order, stopping at the first refusal, and returns how many the
     * socket took. Stopping matters for a chord: once the queue is full the
     * rest of the sequence is going nowhere, and pressing on would leave the
     * guest with whichever fragments happened to fit.
     */
    private fun sendWhileAccepted(events: List<MediaInput>): Int {
        var delivered = 0
        for (event in events) {
            if (!send(event)) break
            delivered += 1
        }
        return delivered
    }

    /**
     * Releases the modifiers a cut-off chord may have left pressed in the
     * guest, innermost first -- the reverse of the order [InputMapper.keyChord]
     * presses them, so the guest sees a hand lifting off the keys.
     */
    private fun unwind(held: HeldModifiers, releaseShift: Boolean) {
        val codes = mutableListOf<Int>()
        if (releaseShift) codes += KeycodeMap.KEY_LEFTSHIFT
        if (held.alt) codes += KeycodeMap.KEY_LEFTALT
        if (held.ctrl) codes += KeycodeMap.KEY_LEFTCTRL
        releaseKeys(codes)
    }

    private fun releaseKeys(evdevCodes: List<Int>) {
        val stream = primary() ?: return
        for (code in evdevCodes) send(InputMapper.keyboardKey(stream.clientId, stream.surfaceId, code, pressed = false))
    }

    private fun publish(next: StickyModifiers) {
        _modifiers.value = next
    }
}
