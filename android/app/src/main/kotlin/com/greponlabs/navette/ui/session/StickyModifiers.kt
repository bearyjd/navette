package com.greponlabs.navette.ui.session

enum class StickyState { Off, Armed, Locked }

/**
 * The two toolbar modifiers; the evdev code is what wprsd's xkb state reacts
 * to. Only the left-hand codes: the right-hand ones map to the same xkb
 * modifier and there is no reason to send two spellings of "Ctrl".
 */
enum class StickyKey(val evdevCode: Int) {
    Ctrl(KeycodeMap.KEY_LEFTCTRL),
    Alt(KeycodeMap.KEY_LEFTALT),
}

/** Which modifiers wrap the next key. A value, like [ViewTransform]. */
data class HeldModifiers(val ctrl: Boolean = false, val alt: Boolean = false) {
    val any: Boolean get() = ctrl || alt

    companion object {
        val NONE = HeldModifiers()
    }
}

/**
 * Tap = armed for the next key, tap again = off; long-press = locked until
 * tapped. [consumed] is what a sent key does to the state: an armed modifier
 * is spent, a locked one stays. Pure and immutable, so the transitions are
 * testable without a controller -- the same shape as [GestureInterpreter]'s
 * `(state, event) -> state` and [ViewTransform]'s "every operation returns a
 * new instance".
 *
 * Lives in the controller, so a reconnect rebuild resets it to [Off]; the
 * key bar's pinned state, by contrast, is session-keyed in the screen and
 * survives. Accepted: a modifier armed across a socket drop is a surprise
 * either way, and Off is the surprise that cannot fire a shortcut.
 */
data class StickyModifiers(
    val ctrl: StickyState = StickyState.Off,
    val alt: StickyState = StickyState.Off,
) {
    fun tapped(key: StickyKey): StickyModifiers =
        update(key) { state -> if (state == StickyState.Off) StickyState.Armed else StickyState.Off }

    fun locked(key: StickyKey): StickyModifiers = update(key) { StickyState.Locked }

    fun consumed(): StickyModifiers = StickyModifiers(ctrl = spend(ctrl), alt = spend(alt))

    fun held(): HeldModifiers = HeldModifiers(ctrl = ctrl != StickyState.Off, alt = alt != StickyState.Off)

    /** Whether [key] is armed or locked, for deciding whether a transition needs a release. */
    fun holds(key: StickyKey): Boolean =
        when (key) {
            StickyKey.Ctrl -> ctrl != StickyState.Off
            StickyKey.Alt -> alt != StickyState.Off
        }

    private fun update(key: StickyKey, next: (StickyState) -> StickyState): StickyModifiers =
        when (key) {
            StickyKey.Ctrl -> copy(ctrl = next(ctrl))
            StickyKey.Alt -> copy(alt = next(alt))
        }

    private fun spend(state: StickyState): StickyState = if (state == StickyState.Armed) StickyState.Off else state
}
