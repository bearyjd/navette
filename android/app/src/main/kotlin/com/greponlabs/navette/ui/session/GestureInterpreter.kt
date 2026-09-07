package com.greponlabs.navette.ui.session

import kotlin.math.abs
import kotlin.math.hypot

/**
 * How long a single finger must stay alone before its left press is sent.
 *
 * A pinch always begins with one finger down, and the press used to go out
 * immediately -- so every pinch started with a stray left click at the first
 * finger's position. Deferring the press by this much lets a second finger
 * cancel it. A tap shorter than this still clicks: see
 * [GestureEffect.EndLeftPress].
 */
const val PRESS_ARM_MS: Long = 60L

/** A two-finger touch shorter than this, and still, is a right-click. */
const val TAP_TIMEOUT_MS: Long = 250L

/** How far the fingers may drift before a two-finger touch stops being a tap. */
const val TAP_SLOP_PX: Float = 24f

/**
 * Fractional change in finger spacing before a two-finger touch starts
 * zooming. Relative, so a wide pinch and a narrow one feel the same.
 */
const val PINCH_SLOP_RATIO: Double = 0.05

/**
 * Absolute change in finger spacing before a two-finger touch starts zooming,
 * required *as well as* [PINCH_SLOP_RATIO]. A relative threshold alone has
 * no floor: fingers 40px apart would latch a pinch on 2px of spacing change,
 * which is inside ordinary digitizer jitter -- and a latched pinch silently
 * cancels the right-click a two-finger tap was about to produce.
 */
const val PINCH_SLOP_PX: Float = 8f

/** One finger, as this screen sees it: an identity and a screen-space position. */
data class TouchPointer(val id: Int, val x: Float, val y: Float)

enum class TouchAction { Down, Move, Up, PointerDown, PointerUp, Cancel }

/**
 * A `MotionEvent` reduced to what the interpreter needs. Positions are in
 * screen space (see [ViewTransform.localToScreen]), so they stay put when the
 * view transform changes under the fingers mid-gesture.
 */
data class TouchEvent(
    val action: TouchAction,
    /** For [TouchAction.PointerDown] and [TouchAction.PointerUp], the finger the action is about. */
    val actionPointerId: Int,
    val pointers: List<TouchPointer>,
    val eventTimeMs: Long,
    /** Latches two-finger drag intent; read only at [TouchAction.PointerDown]. */
    val zoomed: Boolean,
)

/**
 * What the controller should do in response to a step. The interpreter never
 * sends anything itself -- it has no socket, no clock and no view -- which is
 * what keeps every transition testable on the JVM.
 */
sealed interface GestureEffect {
    /** Move the guest pointer to a screen-space position. */
    data class Motion(val x: Float, val y: Float) : GestureEffect

    /** Start the arming timer; the press itself is the controller's to send when it fires. */
    data object ArmLeftPress : GestureEffect

    /** A second finger arrived, or the touch was cancelled: drop the armed press, or release a sent one. */
    data object CancelLeftPress : GestureEffect

    /**
     * The finger lifted. Release a sent press -- or, for a tap shorter than
     * [PRESS_ARM_MS] whose press never fired, send press and release now so a
     * quick tap still clicks. Only the controller knows which, so it decides.
     */
    data object EndLeftPress : GestureEffect

    /** Press and release `BTN_RIGHT`; always preceded by the [Motion] that positions it. */
    data object RightClick : GestureEffect

    /** Multiply the zoom by [scaleFactor] about a screen-space focal point. */
    data class Zoom(val scaleFactor: Double, val focalX: Float, val focalY: Float) : GestureEffect

    /** Translate the zoomed view by a screen-space delta. */
    data class Pan(val dx: Float, val dy: Float) : GestureEffect

    /** Scroll the guest by a screen-space finger delta. */
    data class Scroll(val dx: Float, val dy: Float) : GestureEffect
}

/** Whether a two-finger drag moves the view or the guest's content. Latched for the gesture. */
enum class DragMode { Pan, Scroll }

sealed interface GestureState {
    data object Idle : GestureState

    /** One finger down; it drives the guest pointer. */
    data class OnePointer(val id: Int) : GestureState

    /** Two fingers down; they drive the view. Extra fingers are ignored. */
    data class TwoPointer(
        val idA: Int,
        val idB: Int,
        val mode: DragMode,
        val startTimeMs: Long,
        /** Where the first finger was when the second landed: the right-click anchor. */
        val firstX: Float,
        val firstY: Float,
        val startDistance: Float,
        val startFocalX: Float,
        val startFocalY: Float,
        val lastDistance: Float,
        val lastFocalX: Float,
        val lastFocalY: Float,
        /** Once true, stays true: the touch has stopped being a possible tap. */
        val movedBeyondSlop: Boolean = false,
        /** Once true, stays true: a [GestureEffect.Zoom] has been emitted. */
        val pinching: Boolean = false,
    ) : GestureState

    /**
     * A view gesture has happened; the pointer stays out of it until every
     * finger lifts. Without this, lifting one finger of a pinch would hand
     * control back to the survivor and send a motion and a press into the
     * guest wherever it happened to be.
     */
    data object Suppressed : GestureState
}

data class GestureStep(val state: GestureState, val effects: List<GestureEffect>)

/**
 * The multi-touch state machine, as a pure `(state, event) -> (state, effects)`
 * function over plain values. One finger drives the guest pointer exactly as
 * before this existed; two fingers drive the view (pinch to zoom, drag to pan
 * or scroll) or right-click (a quick, still tap).
 *
 * [DragMode] is decided once, from [TouchEvent.zoomed] at the
 * [TouchAction.PointerDown] that starts the two-finger gesture, and never
 * re-read: zoomed in, a drag pans; at 1:1, it scrolls the guest. While zoomed
 * in the guest therefore cannot be scrolled -- pinch back to 1:1 first. That
 * is a chosen limitation, recorded in `android/README.md`.
 */
object GestureInterpreter {
    fun step(state: GestureState, event: TouchEvent): GestureStep =
        when (state) {
            GestureState.Idle -> stepIdle(event)
            is GestureState.OnePointer -> stepOnePointer(state, event)
            is GestureState.TwoPointer -> stepTwoPointer(state, event)
            GestureState.Suppressed -> stepSuppressed(event)
        }

    private fun stepIdle(event: TouchEvent): GestureStep =
        when (event.action) {
            TouchAction.Down -> begin(event)
            // Anything else without a preceding Down is a stream this screen
            // missed the start of; there is nothing to act on.
            else -> GestureStep(GestureState.Idle, emptyList())
        }

    private fun stepOnePointer(state: GestureState.OnePointer, event: TouchEvent): GestureStep =
        when (event.action) {
            TouchAction.Move -> {
                val pointer = event.pointers.firstOrNull { it.id == state.id }
                GestureStep(state, listOfNotNull(pointer?.let { GestureEffect.Motion(it.x, it.y) }))
            }
            TouchAction.Up -> GestureStep(GestureState.Idle, listOf(GestureEffect.EndLeftPress))
            TouchAction.Cancel -> GestureStep(GestureState.Idle, listOf(GestureEffect.CancelLeftPress))
            TouchAction.PointerDown -> beginTwoPointer(state, event)
            // A fresh Down with a finger still tracked means its Up was
            // missed: unwind the old press before starting over.
            TouchAction.Down -> begin(event, GestureEffect.CancelLeftPress)
            TouchAction.PointerUp -> GestureStep(state, emptyList())
        }

    private fun stepTwoPointer(state: GestureState.TwoPointer, event: TouchEvent): GestureStep =
        when (event.action) {
            TouchAction.Move -> moveTwoPointer(state, event)
            TouchAction.PointerUp ->
                if (event.actionPointerId == state.idA || event.actionPointerId == state.idB) {
                    GestureStep(GestureState.Suppressed, rightClickIfTap(state, event))
                } else {
                    // An ignored extra finger lifting changes nothing.
                    GestureStep(state, emptyList())
                }
            // Extra fingers are ignored; the first two stay tracked.
            TouchAction.PointerDown -> GestureStep(state, emptyList())
            TouchAction.Up, TouchAction.Cancel -> GestureStep(GestureState.Idle, emptyList())
            TouchAction.Down -> begin(event)
        }

    private fun stepSuppressed(event: TouchEvent): GestureStep =
        when (event.action) {
            TouchAction.Up, TouchAction.Cancel -> GestureStep(GestureState.Idle, emptyList())
            TouchAction.Down -> begin(event)
            else -> GestureStep(GestureState.Suppressed, emptyList())
        }

    private fun begin(event: TouchEvent, vararg before: GestureEffect): GestureStep {
        val pointer = event.pointers.firstOrNull() ?: return GestureStep(GestureState.Idle, before.toList())
        return GestureStep(
            GestureState.OnePointer(pointer.id),
            before.toList() + GestureEffect.Motion(pointer.x, pointer.y) + GestureEffect.ArmLeftPress,
        )
    }

    private fun beginTwoPointer(state: GestureState.OnePointer, event: TouchEvent): GestureStep {
        val first = event.pointers.firstOrNull { it.id == state.id }
        val second = event.pointers.firstOrNull { it.id == event.actionPointerId && it.id != state.id }
        // A second finger whose partner is not in the event means the stream
        // this screen is tracking has diverged from Android's. Keeping the
        // phantom OnePointer would let its eventual Up send press+release at
        // a stale position; suppress until every finger lifts instead.
        if (first == null || second == null) {
            return GestureStep(GestureState.Suppressed, listOf(GestureEffect.CancelLeftPress))
        }
        val distance = distance(first, second)
        val focalX = (first.x + second.x) / 2f
        val focalY = (first.y + second.y) / 2f
        return GestureStep(
            GestureState.TwoPointer(
                idA = first.id,
                idB = second.id,
                mode = if (event.zoomed) DragMode.Pan else DragMode.Scroll,
                startTimeMs = event.eventTimeMs,
                firstX = first.x,
                firstY = first.y,
                startDistance = distance,
                startFocalX = focalX,
                startFocalY = focalY,
                lastDistance = distance,
                lastFocalX = focalX,
                lastFocalY = focalY,
            ),
            listOf(GestureEffect.CancelLeftPress),
        )
    }

    private fun moveTwoPointer(state: GestureState.TwoPointer, event: TouchEvent): GestureStep {
        val a = event.pointers.firstOrNull { it.id == state.idA }
        val b = event.pointers.firstOrNull { it.id == state.idB }
        if (a == null || b == null) return GestureStep(state, emptyList())
        val distance = distance(a, b)
        val focalX = (a.x + b.x) / 2f
        val focalY = (a.y + b.y) / 2f
        val effects = mutableListOf<GestureEffect>()

        // Zoom: nothing until the spacing has clearly changed -- by an absolute
        // amount and, when there is a starting spacing to compare against, by
        // a fraction of it -- then the accumulated ratio so the first step
        // lands where the fingers are, then per-move ratios. lastDistance
        // tracks every move regardless. Fingers that landed on the same point
        // have no starting spacing; they latch on the absolute floor alone and
        // take the first non-zero spacing as their reference.
        val spacingChanged =
            abs(distance - state.startDistance) > PINCH_SLOP_PX &&
                (state.startDistance <= 0f || abs(distance / state.startDistance - 1.0) > PINCH_SLOP_RATIO)
        val pinching = state.pinching || spacingChanged
        if (pinching) {
            val reference = if (state.pinching) state.lastDistance else state.startDistance
            if (reference > 0f && distance > 0f && distance != reference) {
                effects += GestureEffect.Zoom((distance / reference).toDouble(), focalX, focalY)
            }
        }

        // Drag: the same shape -- a dead zone, then the accumulated delta,
        // then per-move deltas. A scroll is suppressed once the gesture has
        // become a pinch: a page jumping while it is being zoomed is worse
        // than a pinch that ignores its own drift.
        val movedBeyondSlop =
            state.movedBeyondSlop || hypot(focalX - state.startFocalX, focalY - state.startFocalY) > TAP_SLOP_PX
        if (movedBeyondSlop) {
            val (fromX, fromY) = if (state.movedBeyondSlop) state.lastFocalX to state.lastFocalY else state.startFocalX to state.startFocalY
            val dx = focalX - fromX
            val dy = focalY - fromY
            if (dx != 0f || dy != 0f) {
                when (state.mode) {
                    DragMode.Pan -> effects += GestureEffect.Pan(dx, dy)
                    DragMode.Scroll -> if (!pinching) effects += GestureEffect.Scroll(dx, dy)
                }
            }
        }

        return GestureStep(
            state.copy(
                lastDistance = distance,
                lastFocalX = focalX,
                lastFocalY = focalY,
                movedBeyondSlop = movedBeyondSlop,
                pinching = pinching,
            ),
            effects,
        )
    }

    private fun rightClickIfTap(state: GestureState.TwoPointer, event: TouchEvent): List<GestureEffect> {
        val quick = event.eventTimeMs - state.startTimeMs < TAP_TIMEOUT_MS
        val still = !state.movedBeyondSlop && !state.pinching
        if (!quick || !still) return emptyList()
        return listOf(GestureEffect.Motion(state.firstX, state.firstY), GestureEffect.RightClick)
    }

    private fun distance(a: TouchPointer, b: TouchPointer): Float = hypot(a.x - b.x, a.y - b.y)
}
