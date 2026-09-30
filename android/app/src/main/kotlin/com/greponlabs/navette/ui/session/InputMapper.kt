package com.greponlabs.navette.ui.session

import android.view.KeyEvent
import com.greponlabs.navette.net.BTN_LEFT
import com.greponlabs.navette.net.MAX_VIEWPORT_HEIGHT
import com.greponlabs.navette.net.MAX_VIEWPORT_WIDTH
import com.greponlabs.navette.net.MIN_VIEWPORT_HEIGHT
import com.greponlabs.navette.net.MIN_VIEWPORT_WIDTH
import com.greponlabs.navette.net.MediaInput

/**
 * Characters the hidden IME field may hold before it is emptied: generous for
 * a burst of typing, small enough that a stale buffer never matters. Counted
 * in characters the guest actually received (see [InputMapper.sendableText]),
 * because those are what a later deletion would be diffed against.
 */
internal const val IME_BUFFER_LIMIT: Int = 64

/**
 * Turns Android input into [MediaInput], as pure functions over primitives.
 *
 * Nothing here takes a `MotionEvent` or a `KeyEvent` object: the screen pulls
 * the coordinates and codes out and passes them in, which keeps every one of
 * these testable on the JVM without Robolectric or a device.
 *
 * Field names and semantics follow `crates/navette-bridge/src/input.rs` --
 * `keycode` is a raw evdev code and `button` a raw evdev `BTN_*`, both
 * forwarded to the guest with no server-side translation.
 */
object InputMapper {
    /**
     * Always `0`. Android reports hardware-keyboard keys already translated
     * through the device's own layout, so this client has nothing to switch
     * between and never needs the bridge's layout indirection.
     */
    const val LAYOUT_INDEX: Int = 0

    fun pointerMotion(clientId: Long, surfaceId: Long, x: Double, y: Double): MediaInput.PointerMotion =
        MediaInput.PointerMotion(clientId = clientId.toULong(), surfaceId = surfaceId.toULong(), x = x, y = y)

    /**
     * [button] is a raw evdev `BTN_*` code, forwarded to the guest unchanged.
     * This client sends two: [BTN_LEFT] for a one-finger tap and [BTN_RIGHT]
     * for a two-finger tap. Anything outside `0x110..0x11f` is refused by
     * `MediaInput.validate()` before it reaches the wire, mirroring
     * `media.rs:298-300`.
     *
     * The bridge dispatches a button at `Point { x: 0.0, y: 0.0 }`
     * (`crates/navette-bridge/src/input.rs:119-123`) -- position comes from
     * the most recent [pointerMotion], so one must always precede this.
     */
    fun pointerButton(clientId: Long, surfaceId: Long, button: Int, pressed: Boolean): MediaInput.PointerButton =
        MediaInput.PointerButton(
            clientId = clientId.toULong(),
            surfaceId = surfaceId.toULong(),
            button = button,
            pressed = pressed,
        )

    /**
     * A scroll delta in the guest's own surface-local pixels; see
     * [scrollUnits] for where that unit comes from. Like [pointerButton],
     * dispatched by the bridge at the position of the most recent motion
     * (`input.rs:137-145`), and `PointerAxis` never sets pointer focus on its
     * own (`input.rs:81-90` does that for motion only), so a motion must
     * have preceded it.
     */
    fun pointerAxis(clientId: Long, surfaceId: Long, horizontal: Double, vertical: Double): MediaInput.PointerAxis =
        MediaInput.PointerAxis(
            clientId = clientId.toULong(),
            surfaceId = surfaceId.toULong(),
            horizontal = horizontal,
            vertical = vertical,
        )

    /**
     * Guest scroll pixels per guest pixel of finger travel.
     *
     * The bridge forwards `horizontal`/`vertical` straight through as
     * `AxisScroll.absolute` tagged `AxisSource::Continuous`
     * (`crates/navette-bridge/src/input.rs:132-145`), and wprsd applies that
     * as the `wl_pointer.axis` value verbatim. For a continuous source Wayland
     * defines that value in surface-local pixels, so one pixel of finger
     * travel is one pixel of scroll and the natural ratio is `1.0` -- the
     * content follows the finger, as it does in every native touch UI. The
     * finger delta is first rescaled from surface pixels into the guest's
     * frame with [rescaleToContent], the same way a touch position is: at a
     * logical scale of 2x one surface pixel is half a guest pixel, and
     * without that step the content would run at twice the finger. Held as
     * a constant because it is the one number to tune if a real guest
     * scrolls too fast or too slowly.
     */
    const val SCROLL_UNITS_PER_PIXEL: Double = 1.0

    /**
     * Turns finger travel into a scroll delta. Negated: Wayland's positive
     * axis means "scroll down/right" (reveal lower or further-right content),
     * which is what a finger moving *up* or *left* asks for when the content
     * follows it.
     */
    fun scrollUnits(fingerDelta: Float): Double = -fingerDelta * SCROLL_UNITS_PER_PIXEL

    fun keyboardKey(
        clientId: Long,
        surfaceId: Long,
        evdevCode: Int,
        pressed: Boolean,
    ): MediaInput.KeyboardKey =
        MediaInput.KeyboardKey(
            clientId = clientId.toULong(),
            surfaceId = surfaceId.toULong(),
            keycode = evdevCode,
            pressed = pressed,
        )

    fun keyboardModifiers(clientId: Long, surfaceId: Long, metaState: Int): MediaInput.KeyboardModifiers =
        MediaInput.KeyboardModifiers(
            clientId = clientId.toULong(),
            surfaceId = surfaceId.toULong(),
            ctrl = metaState and KeyEvent.META_CTRL_ON != 0,
            alt = metaState and KeyEvent.META_ALT_ON != 0,
            shift = metaState and KeyEvent.META_SHIFT_ON != 0,
            capsLock = metaState and KeyEvent.META_CAPS_LOCK_ON != 0,
            logo = metaState and KeyEvent.META_META_ON != 0,
            numLock = metaState and KeyEvent.META_NUM_LOCK_ON != 0,
            layoutIndex = LAYOUT_INDEX,
        )

    /** The hardware path derives this from `metaState`; the toolbar supplies it directly. */
    fun heldModifiers(clientId: Long, surfaceId: Long, held: HeldModifiers): MediaInput.KeyboardModifiers =
        MediaInput.KeyboardModifiers(
            clientId = clientId.toULong(),
            surfaceId = surfaceId.toULong(),
            ctrl = held.ctrl,
            alt = held.alt,
            shift = false,
            capsLock = false,
            logo = false,
            numLock = false,
            layoutIndex = LAYOUT_INDEX,
        )

    /**
     * One key, wrapped in whatever modifiers are held.
     *
     * The raw KEY_LEFTCTRL/KEY_LEFTALT press is the part that works: wprsd
     * feeds every `KeyboardEvent::Key` into smithay's xkb state
     * (`src/server/client_handlers.rs:267-300` at the rev pinned in
     * `crates/navetted/Cargo.toml`) and derives ctrl/alt/shift from *that*;
     * its `KeyboardEvent::Modifiers` handler (`:431-468`) only sets the
     * layout and toggles caps/num lock, so `Modifiers{ctrl = true}` on its
     * own is a no-op there. Shift already works this way for the IME path
     * (see [imeTextDelta]). The Modifiers messages are sent anyway to match
     * what a hardware Ctrl produces (`SessionController.onKeyEvent`).
     *
     * Nesting is ctrl outermost, alt inside it, shift innermost; releases
     * mirror the presses, like a hand lifting off the keys in reverse.
     */
    fun keyChord(
        clientId: Long,
        surfaceId: Long,
        evdevCode: Int,
        needsShift: Boolean,
        held: HeldModifiers,
    ): List<MediaInput> {
        val events = mutableListOf<MediaInput>()
        if (held.ctrl) events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_LEFTCTRL, pressed = true)
        if (held.alt) events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_LEFTALT, pressed = true)
        if (held.any) events += heldModifiers(clientId, surfaceId, held)
        if (needsShift) events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_LEFTSHIFT, pressed = true)
        events += keyboardKey(clientId, surfaceId, evdevCode, pressed = true)
        events += keyboardKey(clientId, surfaceId, evdevCode, pressed = false)
        if (needsShift) events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_LEFTSHIFT, pressed = false)
        if (held.any) events += heldModifiers(clientId, surfaceId, HeldModifiers.NONE)
        if (held.alt) events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_LEFTALT, pressed = false)
        if (held.ctrl) events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_LEFTCTRL, pressed = false)
        return events
    }

    /**
     * The subsequence of [text] this client can actually type -- characters
     * with no evdev mapping removed.
     *
     * This is what the guest's buffer ends up holding after
     * [imeTextDelta] has typed [text], which is why the delta must diff two of
     * these rather than two raw field values.
     */
    fun sendableText(text: String): String = text.filter { KeycodeMap.asciiCharToEvdev(it) != null }

    /**
     * Whether the hidden IME field should be emptied after this edit.
     *
     * Two reasons to empty it. A [chorded] character is one the guest received
     * as a chord rather than as text (Gboard committed "c" for Ctrl+C), so a
     * later Backspace must not be diffed against it. And a buffer past
     * [IME_BUFFER_LIMIT] is dead weight: the field is invisible and only its
     * diff matters.
     *
     * Never while [hasComposition]: emptying the field restarts the IME's
     * input session, which would drop the composition in flight. With the
     * password-typed field Gboard does not compose, so in practice the guard
     * is always open -- but another keyboard may, and the buffer running long
     * is harmless where a dropped composition is not.
     *
     * A function rather than a branch inside the field's `onValueChange`: that
     * lambda cannot be reached from a JVM test, and this is the rule that
     * decides whether typing stays in step with the guest.
     */
    fun shouldResetImeBuffer(text: String, hasComposition: Boolean, chorded: Boolean): Boolean {
        if (hasComposition) return false
        return chorded || sendableText(text).length > IME_BUFFER_LIMIT
    }

    /** How many characters [imeTextDelta] types for this edit -- zero for a pure deletion. */
    fun typedChars(previous: String, current: String): Int {
        val sentPrevious = sendableText(previous)
        val sentCurrent = sendableText(current)
        return sentCurrent.length - commonPrefixLength(sentPrevious, sentCurrent)
    }

    /**
     * Turns an on-screen-keyboard edit into the key events that would have
     * produced it.
     *
     * **Both sides are filtered through [sendableText] first**, and that is
     * load-bearing rather than tidiness. The guest never received the
     * characters this client cannot map, so counting backspaces against the
     * raw [previous] would delete more than was ever typed -- and the two
     * halves would stay out of step, since nothing re-syncs them. It is
     * reachable in ordinary English: an IME that substitutes a curly
     * apostrophe puts an untypable character in the field, and the next
     * deletion then eats a character the user meant to keep. Diffing what was
     * actually sent keeps the count honest.
     *
     * The diff itself is common-prefix only: everything past the shared prefix
     * is backspaced away, then the rest of [current] is typed. That handles
     * appends and deletions exactly, and handles a mid-string replacement (an
     * autocomplete swapping "teh" for "the") correctly but inefficiently -- it
     * retypes the tail rather than editing in place. **Accepted
     * simplification**: a minimal-edit diff would need cursor-position
     * tracking this slice's hidden-field IME approach does not have, and the
     * visible result is the same.
     *
     * **Only the first typed character goes out under [held]**, and the
     * backspaces never do. A sticky modifier is armed "for the next key",
     * which is one key: an edit can carry several characters at once -- a
     * paste, or a keyboard that commits a whole word -- and chording each of
     * them turned a paste of "abc" under armed Ctrl into Ctrl+A, Ctrl+B,
     * Ctrl+C, where Ctrl+A alone is select-all in most guests. A deletion is
     * the IME reconciling its own buffer rather than the user asking for
     * Ctrl+Backspace; that request arrives through the key bar path instead.
     */
    fun imeTextDelta(
        clientId: Long,
        surfaceId: Long,
        previous: String,
        current: String,
        held: HeldModifiers = HeldModifiers.NONE,
    ): List<MediaInput> {
        val sentPrevious = sendableText(previous)
        val sentCurrent = sendableText(current)
        val shared = commonPrefixLength(sentPrevious, sentCurrent)
        val events = mutableListOf<MediaInput>()

        repeat(sentPrevious.length - shared) {
            events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_BACKSPACE, pressed = true)
            events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_BACKSPACE, pressed = false)
        }

        // Tracked rather than `index == shared` so an unmapped character at the
        // head of the edit does not spend the chord on nothing.
        var chorded = false
        for (index in shared until sentCurrent.length) {
            // Non-null by construction: sentCurrent only holds mapped characters.
            val (evdevCode, needsShift) = KeycodeMap.asciiCharToEvdev(sentCurrent[index]) ?: continue
            events += keyChord(clientId, surfaceId, evdevCode, needsShift, if (chorded) HeldModifiers.NONE else held)
            chorded = true
        }

        return events
    }

    /**
     * Rescales a touch position from live surface-pixel space into the
     * currently-displayed frame's own coordinate space.
     *
     * A port of `rescale_to_content` in
     * `crates/navette-viewer/src/native.rs:97-108`. In the steady state the
     * two sizes are identical -- the surface reports its own pixel size as the
     * viewport and the bridge re-encodes at that size, so there is no
     * letterbox. They disagree only between a resize being sent and the first
     * frame at the new size arriving, which is exactly the window in which a
     * tap used to land in the wrong place.
     *
     * `null` when either surface dimension is degenerate: a view can
     * transiently report zero size mid-layout.
     */
    fun rescaleToContent(
        rawX: Float,
        rawY: Float,
        surfaceWidth: Int,
        surfaceHeight: Int,
        contentWidth: Int,
        contentHeight: Int,
    ): Pair<Double, Double>? {
        if (surfaceWidth <= 0 || surfaceHeight <= 0) return null
        val scaleX = contentWidth.toDouble() / surfaceWidth.toDouble()
        val scaleY = contentHeight.toDouble() / surfaceHeight.toDouble()
        return (rawX * scaleX) to (rawY * scaleY)
    }

    /**
     * The viewport size to report for a surface of [width]x[height] pixels,
     * or `null` for a degenerate size that should not be reported at all.
     *
     * Clamped into the bounds `media.rs:307-311` validates against, then
     * rounded down to even -- H.264's 4:2:0 chroma subsampling wants even
     * dimensions, and every bound is itself even, so rounding after clamping
     * can never leave the range. Sending a value the bridge would reject is
     * never right when a valid nearby one exists.
     */
    fun clampViewport(width: Int, height: Int): Pair<Int, Int>? {
        if (width <= 0 || height <= 0) return null
        val clampedWidth = width.coerceIn(MIN_VIEWPORT_WIDTH, MAX_VIEWPORT_WIDTH)
        val clampedHeight = height.coerceIn(MIN_VIEWPORT_HEIGHT, MAX_VIEWPORT_HEIGHT)
        return (clampedWidth and 1.inv()) to (clampedHeight and 1.inv())
    }

    /**
     * The viewport to report for a [width]x[height] surface viewed at
     * [factor] -- the logical scale: the guest lays out for a screen that
     * many times smaller and MediaCodec's scale-to-fit stretches the decoded
     * frame back over the whole surface.
     *
     * Divides by the largest divisor <= [factor] that keeps both dimensions
     * at or above the bridge's minimum (`crates/navette-protocol/src/media.rs:416-420`:
     * 320x240, mirrored by [MIN_VIEWPORT_WIDTH]/[MIN_VIEWPORT_HEIGHT]), so
     * the clamp in [clampViewport] never changes the aspect ratio -- a
     * clamped height with an unclamped width would make that scale-to-fit
     * stretch the picture. Reachable in practice: a landscape phone with the
     * IME up has ~450 px of surface height, and 450/2 < 240. At `1f` this is
     * exactly [clampViewport]. `null` for a degenerate surface or a factor
     * that is not a real number >= 1.
     */
    fun scaledViewport(width: Int, height: Int, factor: Float): Pair<Int, Int>? {
        if (width <= 0 || height <= 0 || !factor.isFinite() || factor < 1f) return null
        val fit =
            minOf(factor.toDouble(), width.toDouble() / MIN_VIEWPORT_WIDTH, height.toDouble() / MIN_VIEWPORT_HEIGHT)
                .coerceAtLeast(1.0)
        return clampViewport((width / fit).toInt(), (height / fit).toInt())
    }

    private fun commonPrefixLength(first: String, second: String): Int {
        val limit = minOf(first.length, second.length)
        var index = 0
        while (index < limit && first[index] == second[index]) index++
        return index
    }
}
