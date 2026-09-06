package com.greponlabs.navette.ui.session

import android.view.KeyEvent
import com.greponlabs.navette.net.BTN_LEFT
import com.greponlabs.navette.net.MAX_VIEWPORT_HEIGHT
import com.greponlabs.navette.net.MAX_VIEWPORT_WIDTH
import com.greponlabs.navette.net.MIN_VIEWPORT_HEIGHT
import com.greponlabs.navette.net.MIN_VIEWPORT_WIDTH
import com.greponlabs.navette.net.MediaInput

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
     * Guest scroll pixels per pixel of finger travel.
     *
     * The bridge forwards `horizontal`/`vertical` straight through as
     * `AxisScroll.absolute` tagged `AxisSource::Continuous`
     * (`crates/navette-bridge/src/input.rs:132-145`), and wprsd applies that
     * as the `wl_pointer.axis` value verbatim. For a continuous source Wayland
     * defines that value in surface-local pixels, so one pixel of finger
     * travel is one pixel of scroll and the natural ratio is `1.0` -- the
     * content follows the finger, as it does in every native touch UI. Held
     * as a constant because it is the one number to tune if a real guest
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
     */
    fun imeTextDelta(
        clientId: Long,
        surfaceId: Long,
        previous: String,
        current: String,
    ): List<MediaInput> {
        val sentPrevious = sendableText(previous)
        val sentCurrent = sendableText(current)
        val shared = commonPrefixLength(sentPrevious, sentCurrent)
        val events = mutableListOf<MediaInput>()

        repeat(sentPrevious.length - shared) {
            events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_BACKSPACE, pressed = true)
            events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_BACKSPACE, pressed = false)
        }

        for (index in shared until sentCurrent.length) {
            // Non-null by construction: sentCurrent only holds mapped characters.
            val (evdevCode, needsShift) = KeycodeMap.asciiCharToEvdev(sentCurrent[index]) ?: continue
            if (needsShift) {
                events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_LEFTSHIFT, pressed = true)
            }
            events += keyboardKey(clientId, surfaceId, evdevCode, pressed = true)
            events += keyboardKey(clientId, surfaceId, evdevCode, pressed = false)
            if (needsShift) {
                events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_LEFTSHIFT, pressed = false)
            }
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

    private fun commonPrefixLength(first: String, second: String): Int {
        val limit = minOf(first.length, second.length)
        var index = 0
        while (index < limit && first[index] == second[index]) index++
        return index
    }
}
