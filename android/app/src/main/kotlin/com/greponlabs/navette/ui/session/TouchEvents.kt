package com.greponlabs.navette.ui.session

import android.view.MotionEvent

/**
 * Reduces a raw `MotionEvent` to the [TouchEvent] [GestureInterpreter]
 * steps on, or `null` for an action the interpreter does not know (the
 * View system keeps those).
 *
 * Pointers are lifted into screen space through [transform] first. The
 * framework has already inverse-mapped them through the view's transform, so
 * while a pinch is changing that transform, the local coordinates of a
 * finger that has not moved would change under it -- a feedback loop. Screen
 * space is where the fingers physically are, and stays put. A function of
 * the event and the transform alone, so the controller's touch path is the
 * one line that steps the interpreter.
 */
internal fun MotionEvent.toTouchEvent(transform: ViewTransform): TouchEvent? {
    val action = touchAction(actionMasked) ?: return null
    val pointers =
        List(pointerCount) { index ->
            val (x, y) = transform.localToScreen(getX(index), getY(index))
            TouchPointer(getPointerId(index), x.toFloat(), y.toFloat())
        }
    return TouchEvent(
        action = action,
        actionPointerId = getPointerId(actionIndex),
        pointers = pointers,
        eventTimeMs = eventTime,
        zoomed = transform.isZoomed,
    )
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
