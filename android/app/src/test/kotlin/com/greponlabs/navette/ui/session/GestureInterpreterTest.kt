package com.greponlabs.navette.ui.session

import org.junit.Assert.assertEquals
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

private const val FINGER_A = 0
private const val FINGER_B = 1
private const val FINGER_C = 2

/**
 * Drives [GestureInterpreter] the way the controller does -- threading the
 * state through -- and records every effect, so a test reads as the gesture
 * it describes.
 */
private class Fingers {
    var state: GestureState = GestureState.Idle
        private set
    val effects = mutableListOf<GestureEffect>()
    private var now = 1_000L
    private var zoomed = false

    fun zoomed(value: Boolean) = apply { zoomed = value }

    fun after(ms: Long) = apply { now += ms }

    fun down(id: Int, x: Float, y: Float) = send(TouchAction.Down, id, listOf(TouchPointer(id, x, y)))

    fun pointerDown(id: Int, vararg all: TouchPointer) = send(TouchAction.PointerDown, id, all.toList())

    fun move(vararg all: TouchPointer) = send(TouchAction.Move, all.first().id, all.toList())

    fun pointerUp(id: Int, vararg all: TouchPointer) = send(TouchAction.PointerUp, id, all.toList())

    fun up(id: Int, x: Float, y: Float) = send(TouchAction.Up, id, listOf(TouchPointer(id, x, y)))

    fun cancel(id: Int, x: Float, y: Float) = send(TouchAction.Cancel, id, listOf(TouchPointer(id, x, y)))

    /** Effects since the last call, cleared on read. */
    fun drain(): List<GestureEffect> = effects.toList().also { effects.clear() }

    private fun send(action: TouchAction, actionId: Int, pointers: List<TouchPointer>): Fingers {
        val step = GestureInterpreter.step(state, TouchEvent(action, actionId, pointers, now, zoomed))
        state = step.state
        effects += step.effects
        return this
    }
}

private fun p(id: Int, x: Float, y: Float) = TouchPointer(id, x, y)

class GestureInterpreterTest {
    // -- one finger: the guest pointer, exactly as before --------------------

    @Test
    fun `one finger down moves the pointer and arms the press without sending it`() {
        val fingers = Fingers().down(FINGER_A, 10f, 20f)

        assertEquals(listOf(GestureEffect.Motion(10f, 20f), GestureEffect.ArmLeftPress), fingers.drain())
        assertEquals(GestureState.OnePointer(FINGER_A), fingers.state)
    }

    @Test
    fun `one finger drag emits motion only`() {
        val fingers = Fingers().down(FINGER_A, 10f, 20f)
        fingers.drain()

        fingers.move(p(FINGER_A, 15f, 25f)).move(p(FINGER_A, 30f, 40f))

        assertEquals(listOf(GestureEffect.Motion(15f, 25f), GestureEffect.Motion(30f, 40f)), fingers.drain())
    }

    @Test
    fun `one finger up ends the press and returns to idle`() {
        val fingers = Fingers().down(FINGER_A, 10f, 20f)
        fingers.drain()

        fingers.up(FINGER_A, 10f, 20f)

        assertEquals(listOf(GestureEffect.EndLeftPress), fingers.drain())
        assertSame(GestureState.Idle, fingers.state)
    }

    @Test
    fun `cancel unwinds the press cleanly`() {
        val fingers = Fingers().down(FINGER_A, 10f, 20f)
        fingers.drain()

        fingers.cancel(FINGER_A, 10f, 20f)

        assertEquals(listOf(GestureEffect.CancelLeftPress), fingers.drain())
        assertSame(GestureState.Idle, fingers.state)
    }

    @Test
    fun `a fresh down while a finger is still tracked unwinds the old press first`() {
        val fingers = Fingers().down(FINGER_A, 10f, 20f)
        fingers.drain()

        fingers.down(FINGER_B, 50f, 60f)

        assertEquals(
            listOf(GestureEffect.CancelLeftPress, GestureEffect.Motion(50f, 60f), GestureEffect.ArmLeftPress),
            fingers.drain(),
        )
        assertEquals(GestureState.OnePointer(FINGER_B), fingers.state)
    }

    @Test
    fun `a move for an untracked finger is ignored`() {
        val fingers = Fingers().down(FINGER_A, 10f, 20f)
        fingers.drain()

        fingers.move(p(FINGER_B, 99f, 99f))

        assertEquals(emptyList<GestureEffect>(), fingers.drain())
    }

    // -- the second finger --------------------------------------------------

    @Test
    fun `a second finger cancels the press and starts a two-finger gesture`() {
        val fingers = Fingers().down(FINGER_A, 100f, 100f)
        fingers.drain()

        fingers.pointerDown(FINGER_B, p(FINGER_A, 100f, 100f), p(FINGER_B, 300f, 100f))

        assertEquals(listOf(GestureEffect.CancelLeftPress), fingers.drain())
        assertTrue(fingers.state is GestureState.TwoPointer)
    }

    @Test
    fun `the drag mode is latched from the zoom level when the second finger lands`() {
        val atFit = Fingers().zoomed(false).down(FINGER_A, 100f, 100f)
        atFit.pointerDown(FINGER_B, p(FINGER_A, 100f, 100f), p(FINGER_B, 300f, 100f))
        assertEquals(DragMode.Scroll, (atFit.state as GestureState.TwoPointer).mode)

        val zoomedIn = Fingers().zoomed(true).down(FINGER_A, 100f, 100f)
        zoomedIn.pointerDown(FINGER_B, p(FINGER_A, 100f, 100f), p(FINGER_B, 300f, 100f))
        assertEquals(DragMode.Pan, (zoomedIn.state as GestureState.TwoPointer).mode)
    }

    // -- pinch ---------------------------------------------------------------

    @Test
    fun `spreading the fingers emits a zoom by the accumulated ratio about the focal point`() {
        val fingers = twoFingersDown(zoomed = false)

        // 200px apart -> 400px apart: ratio 2.0, focal stays at (200, 100).
        fingers.move(p(FINGER_A, 0f, 100f), p(FINGER_B, 400f, 100f))

        val zooms = fingers.drain().filterIsInstance<GestureEffect.Zoom>()
        assertEquals(1, zooms.size)
        assertEquals(2.0, zooms.single().scaleFactor, 1e-6)
        assertEquals(200f, zooms.single().focalX, 1e-4f)
        assertEquals(100f, zooms.single().focalY, 1e-4f)
    }

    @Test
    fun `after the first zoom each move emits the ratio since the previous move`() {
        val fingers = twoFingersDown(zoomed = false)
        fingers.move(p(FINGER_A, 0f, 100f), p(FINGER_B, 400f, 100f))
        fingers.drain()

        // 400 -> 500: ratio 1.25 relative to the last move, not 2.5 to the start.
        fingers.move(p(FINGER_A, 0f, 100f), p(FINGER_B, 500f, 100f))

        val zoom = fingers.drain().filterIsInstance<GestureEffect.Zoom>().single()
        assertEquals(1.25, zoom.scaleFactor, 1e-6)
    }

    @Test
    fun `a spacing change inside the pinch slop emits no zoom`() {
        val fingers = twoFingersDown(zoomed = false)

        // 200 -> 202: a 1% change, under the 5% slop.
        fingers.move(p(FINGER_A, 100f, 100f), p(FINGER_B, 302f, 100f))

        assertEquals(emptyList<GestureEffect>(), fingers.drain().filterIsInstance<GestureEffect.Zoom>())
    }

    // -- two-finger drag -----------------------------------------------------

    @Test
    fun `a two-finger drag pans when zoomed in`() {
        val fingers = twoFingersDown(zoomed = true)

        // Both fingers move 50px right and 30px down: past the tap slop.
        fingers.move(p(FINGER_A, 150f, 130f), p(FINGER_B, 350f, 130f))

        val effects = fingers.drain()
        assertEquals(listOf(GestureEffect.Pan(50f, 30f)), effects)
    }

    @Test
    fun `a two-finger drag scrolls the guest at one to one`() {
        val fingers = twoFingersDown(zoomed = false)

        fingers.move(p(FINGER_A, 150f, 130f), p(FINGER_B, 350f, 130f))

        assertEquals(listOf(GestureEffect.Scroll(50f, 30f)), fingers.drain())
    }

    @Test
    fun `after the slop each move emits the delta since the previous move`() {
        val fingers = twoFingersDown(zoomed = true)
        fingers.move(p(FINGER_A, 150f, 130f), p(FINGER_B, 350f, 130f))
        fingers.drain()

        fingers.move(p(FINGER_A, 160f, 130f), p(FINGER_B, 360f, 130f))

        assertEquals(listOf(GestureEffect.Pan(10f, 0f)), fingers.drain())
    }

    @Test
    fun `a drag inside the tap slop emits nothing`() {
        val fingers = twoFingersDown(zoomed = true)

        fingers.move(p(FINGER_A, 105f, 100f), p(FINGER_B, 305f, 100f))

        assertEquals(emptyList<GestureEffect>(), fingers.drain())
    }

    @Test
    fun `the drag mode never flips mid-gesture`() {
        val fingers = twoFingersDown(zoomed = false)

        // The controller would report zoomed=true once a pinch had taken
        // effect; the latch must not care.
        fingers.zoomed(true).move(p(FINGER_A, 150f, 130f), p(FINGER_B, 350f, 130f))

        assertEquals(listOf(GestureEffect.Scroll(50f, 30f)), fingers.drain())
    }

    @Test
    fun `a pinch at one to one does not also scroll the guest`() {
        val fingers = twoFingersDown(zoomed = false)

        // Spread to 400px and drift the focal by 50px in the same move.
        fingers.move(p(FINGER_A, 50f, 130f), p(FINGER_B, 450f, 130f))

        val effects = fingers.drain()
        assertEquals(1, effects.filterIsInstance<GestureEffect.Zoom>().size)
        assertEquals(emptyList<GestureEffect>(), effects.filterIsInstance<GestureEffect.Scroll>())
    }

    @Test
    fun `a pinch while zoomed in still pans with the focal point`() {
        val fingers = twoFingersDown(zoomed = true)

        fingers.move(p(FINGER_A, 50f, 130f), p(FINGER_B, 450f, 130f))

        val effects = fingers.drain()
        assertEquals(1, effects.filterIsInstance<GestureEffect.Zoom>().size)
        assertEquals(listOf(GestureEffect.Pan(50f, 30f)), effects.filterIsInstance<GestureEffect.Pan>())
    }

    // -- two-finger tap ------------------------------------------------------

    @Test
    fun `a quick still two-finger tap right-clicks at the first finger`() {
        val fingers = twoFingersDown(zoomed = false)

        fingers.after(100).pointerUp(FINGER_B, p(FINGER_A, 100f, 100f), p(FINGER_B, 300f, 100f))

        assertEquals(listOf(GestureEffect.Motion(100f, 100f), GestureEffect.RightClick), fingers.drain())
        assertSame(GestureState.Suppressed, fingers.state)
    }

    @Test
    fun `a slow two-finger touch is not a right-click`() {
        val fingers = twoFingersDown(zoomed = false)

        fingers.after(TAP_TIMEOUT_MS).pointerUp(FINGER_B, p(FINGER_A, 100f, 100f), p(FINGER_B, 300f, 100f))

        assertEquals(emptyList<GestureEffect>(), fingers.drain())
        assertSame(GestureState.Suppressed, fingers.state)
    }

    // -- two-finger long-press (HUD toggle) ----------------------------------

    @Test
    fun `two still fingers held past the tap timeout toggle the hud`() {
        val fingers = twoFingersDown(zoomed = false)

        fingers.after(TAP_TIMEOUT_MS + 1).pointerUp(FINGER_B, p(FINGER_A, 100f, 100f), p(FINGER_B, 300f, 100f))

        assertEquals(listOf<GestureEffect>(GestureEffect.ToggleHud), fingers.drain())
        assertSame(GestureState.Suppressed, fingers.state)
    }

    @Test
    fun `fingers that moved do not toggle the hud however long they were down`() {
        val fingers = twoFingersDown(zoomed = false)
        fingers.move(p(FINGER_A, 150f, 130f), p(FINGER_B, 350f, 130f))
        fingers.drain()

        fingers.after(TAP_TIMEOUT_MS + 1).pointerUp(FINGER_A, p(FINGER_A, 150f, 130f), p(FINGER_B, 350f, 130f))

        assertEquals(emptyList<GestureEffect>(), fingers.drain())
    }

    @Test
    fun `a moved two-finger touch is not a right-click`() {
        val fingers = twoFingersDown(zoomed = false)
        fingers.move(p(FINGER_A, 150f, 130f), p(FINGER_B, 350f, 130f))
        fingers.drain()

        fingers.after(100).pointerUp(FINGER_B, p(FINGER_A, 150f, 130f), p(FINGER_B, 350f, 130f))

        assertEquals(emptyList<GestureEffect>(), fingers.drain())
    }

    @Test
    fun `a pinch is not a right-click however quick`() {
        val fingers = twoFingersDown(zoomed = false)
        fingers.move(p(FINGER_A, 0f, 100f), p(FINGER_B, 400f, 100f))
        fingers.drain()

        fingers.after(50).pointerUp(FINGER_B, p(FINGER_A, 0f, 100f), p(FINGER_B, 400f, 100f))

        assertEquals(emptyList<GestureEffect>(), fingers.drain())
    }

    // -- suppression and extra fingers -------------------------------------

    @Test
    fun `lifting one finger of a pair does not hand the survivor to the pointer`() {
        val fingers = twoFingersDown(zoomed = false)
        fingers.after(TAP_TIMEOUT_MS).pointerUp(FINGER_B, p(FINGER_A, 100f, 100f), p(FINGER_B, 300f, 100f))
        fingers.drain()

        fingers.move(p(FINGER_A, 500f, 500f))

        assertEquals(emptyList<GestureEffect>(), fingers.drain())
        assertSame(GestureState.Suppressed, fingers.state)
    }

    @Test
    fun `the last finger lifting after a two-finger gesture returns to idle silently`() {
        val fingers = twoFingersDown(zoomed = false)
        fingers.after(TAP_TIMEOUT_MS).pointerUp(FINGER_B, p(FINGER_A, 100f, 100f), p(FINGER_B, 300f, 100f))
        fingers.drain()

        fingers.up(FINGER_A, 100f, 100f)

        assertEquals(emptyList<GestureEffect>(), fingers.drain())
        assertSame(GestureState.Idle, fingers.state)
    }

    @Test
    fun `a third finger is ignored and the first two stay tracked`() {
        val fingers = twoFingersDown(zoomed = true)
        val before = fingers.state

        fingers.pointerDown(FINGER_C, p(FINGER_A, 100f, 100f), p(FINGER_B, 300f, 100f), p(FINGER_C, 700f, 700f))
        assertEquals(emptyList<GestureEffect>(), fingers.drain())
        assertEquals(before, fingers.state)

        // The tracked pair still drives the gesture with the extra finger present.
        fingers.move(p(FINGER_A, 150f, 130f), p(FINGER_B, 350f, 130f), p(FINGER_C, 700f, 700f))
        assertEquals(listOf(GestureEffect.Pan(50f, 30f)), fingers.drain())
    }

    @Test
    fun `an ignored extra finger lifting changes nothing`() {
        val fingers = twoFingersDown(zoomed = true)
        fingers.pointerDown(FINGER_C, p(FINGER_A, 100f, 100f), p(FINGER_B, 300f, 100f), p(FINGER_C, 700f, 700f))
        val before = fingers.state

        fingers.pointerUp(FINGER_C, p(FINGER_A, 100f, 100f), p(FINGER_B, 300f, 100f), p(FINGER_C, 700f, 700f))

        assertEquals(emptyList<GestureEffect>(), fingers.drain())
        assertEquals(before, fingers.state)
    }

    /**
     * After a finger lifts Android re-indexes the survivors; the interpreter
     * resolves fingers by id, so the order they arrive in must not matter.
     */
    @Test
    fun `fingers are resolved by id not by position in the list`() {
        val fingers = twoFingersDown(zoomed = true)

        fingers.move(p(FINGER_B, 350f, 130f), p(FINGER_A, 150f, 130f))

        assertEquals(listOf(GestureEffect.Pan(50f, 30f)), fingers.drain())
    }

    /**
     * A real touchscreen emits several moves during a 100ms two-finger tap,
     * each with a pixel or two of jitter. With fingers close together that
     * jitter is a large *fraction* of the spacing, so a relative-only pinch
     * threshold would latch a pinch and silently swallow the right-click.
     */
    @Test
    fun `a jittery two-finger tap with close fingers still right-clicks`() {
        val fingers = Fingers().zoomed(false).down(FINGER_A, 100f, 100f)
        fingers.pointerDown(FINGER_B, p(FINGER_A, 100f, 100f), p(FINGER_B, 140f, 100f))
        fingers.drain()

        // Spacing wobbles 40 -> 42 -> 38 -> 41: up to 5% relative, 2px absolute.
        fingers.after(20).move(p(FINGER_A, 99f, 101f), p(FINGER_B, 141f, 100f))
        fingers.after(20).move(p(FINGER_A, 101f, 100f), p(FINGER_B, 139f, 101f))
        fingers.after(20).move(p(FINGER_A, 100f, 99f), p(FINGER_B, 141f, 100f))
        assertEquals(emptyList<GestureEffect>(), fingers.drain().filterIsInstance<GestureEffect.Zoom>())

        fingers.after(20).pointerUp(FINGER_B, p(FINGER_A, 100f, 99f), p(FINGER_B, 141f, 100f))

        assertEquals(listOf(GestureEffect.Motion(100f, 100f), GestureEffect.RightClick), fingers.drain())
    }

    @Test
    fun `a genuine pinch with close fingers still zooms`() {
        val fingers = Fingers().zoomed(false).down(FINGER_A, 100f, 100f)
        fingers.pointerDown(FINGER_B, p(FINGER_A, 100f, 100f), p(FINGER_B, 140f, 100f))
        fingers.drain()

        // 40px -> 60px: 20px absolute, 50% relative -- unmistakably a pinch.
        fingers.move(p(FINGER_A, 90f, 100f), p(FINGER_B, 150f, 100f))

        val zoom = fingers.drain().filterIsInstance<GestureEffect.Zoom>().single()
        assertEquals(1.5, zoom.scaleFactor, 1e-6)
    }

    @Test
    fun `fingers that land on the same point can still start a pinch`() {
        val fingers = Fingers().zoomed(false).down(FINGER_A, 100f, 100f)
        fingers.pointerDown(FINGER_B, p(FINGER_A, 100f, 100f), p(FINGER_B, 100f, 100f))
        fingers.drain()

        // First spread: latches, but there is no starting spacing to make a
        // ratio from, so no zoom yet.
        fingers.move(p(FINGER_A, 90f, 100f), p(FINGER_B, 110f, 100f))
        assertEquals(emptyList<GestureEffect>(), fingers.drain().filterIsInstance<GestureEffect.Zoom>())

        // Second spread: 20px -> 40px against the first non-zero spacing.
        fingers.move(p(FINGER_A, 80f, 100f), p(FINGER_B, 120f, 100f))
        val zoom = fingers.drain().filterIsInstance<GestureEffect.Zoom>().single()
        assertEquals(2.0, zoom.scaleFactor, 1e-6)
    }

    @Test
    fun `a second finger whose partner is missing suppresses the gesture instead of leaving a phantom`() {
        val fingers = Fingers().down(FINGER_A, 10f, 20f)
        fingers.drain()

        // Android reports a second finger, but the first is not in the event.
        fingers.pointerDown(FINGER_B, p(FINGER_B, 300f, 100f))

        assertEquals(listOf(GestureEffect.CancelLeftPress), fingers.drain())
        assertSame(GestureState.Suppressed, fingers.state)

        // The eventual lift must not click at a stale position.
        fingers.up(FINGER_B, 300f, 100f)
        assertEquals(emptyList<GestureEffect>(), fingers.drain())
        assertSame(GestureState.Idle, fingers.state)
    }

    @Test
    fun `events without a preceding down are ignored while idle`() {
        val fingers = Fingers()

        fingers.move(p(FINGER_A, 1f, 1f)).up(FINGER_A, 1f, 1f).cancel(FINGER_A, 1f, 1f)

        assertEquals(emptyList<GestureEffect>(), fingers.drain())
        assertSame(GestureState.Idle, fingers.state)
    }

    /** Finger A at (100, 100), finger B at (300, 100): 200px apart, focal (200, 100). Effects drained. */
    private fun twoFingersDown(zoomed: Boolean): Fingers {
        val fingers = Fingers().zoomed(zoomed).down(FINGER_A, 100f, 100f)
        fingers.pointerDown(FINGER_B, p(FINGER_A, 100f, 100f), p(FINGER_B, 300f, 100f))
        fingers.drain()
        return fingers
    }
}
