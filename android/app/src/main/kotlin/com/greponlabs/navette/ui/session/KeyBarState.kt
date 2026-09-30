package com.greponlabs.navette.ui.session

/**
 * Whether the key bar is on screen, as two independent wishes rather than one
 * flag: [pinned] is "I want the bar with no keyboard" (arrow keys in a pager),
 * [dismissed] is "I want this keyboard without the bar".
 *
 * Both are needed because the bar comes up with the IME by default. With a
 * single `pinned` flag the Keys button was a no-op whenever the keyboard was
 * already showing the bar -- it flipped its own label and changed nothing on
 * screen. [toggled] is defined so that it always changes [visible], which is
 * what the button promises; `KeyBarStateTest` asserts that exhaustively.
 *
 * A value, like [StickyModifiers] and [ViewTransform]: every operation
 * returns a new instance.
 */
internal data class KeyBarState(val pinned: Boolean = false, val dismissed: Boolean = false) {
    fun visible(imeRaised: Boolean): Boolean = pinned || (imeRaised && !dismissed)

    /**
     * What the Keys button does. Hiding always clears both wishes, so the bar
     * goes away whether it was pinned or riding the keyboard. Showing pins it
     * only when there is no keyboard to ride -- otherwise the bar would
     * outlive the IME that brought it up, which is not what the tap asked for.
     *
     * A dismissal lasts for the session (this state is remembered per
     * session): someone who does not want the bar should not have to dismiss
     * it again every time the keyboard comes up.
     */
    fun toggled(imeRaised: Boolean): KeyBarState =
        when {
            visible(imeRaised) -> KeyBarState(pinned = false, dismissed = true)
            imeRaised -> KeyBarState(pinned = false, dismissed = false)
            else -> KeyBarState(pinned = true, dismissed = false)
        }
}
