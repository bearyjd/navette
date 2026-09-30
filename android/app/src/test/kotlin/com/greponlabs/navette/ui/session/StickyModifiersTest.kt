package com.greponlabs.navette.ui.session

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Drives the pure state the way `Fingers` drives [GestureInterpreter]: each
 * step returns the next value, so a transition is asserted by comparing
 * values rather than by observing a controller.
 */
class StickyModifiersTest {
    @Test
    fun `a tap arms a modifier and a second tap disarms it`() {
        val armed = StickyModifiers().tapped(StickyKey.Ctrl)
        assertEquals(StickyState.Armed, armed.ctrl)
        assertEquals(StickyState.Off, armed.alt)

        val disarmed = armed.tapped(StickyKey.Ctrl)
        assertEquals(StickyModifiers(), disarmed)
    }

    @Test
    fun `a long-press locks and a tap unlocks`() {
        val locked = StickyModifiers().locked(StickyKey.Alt)
        assertEquals(StickyState.Locked, locked.alt)

        assertEquals(StickyModifiers(), locked.tapped(StickyKey.Alt))
    }

    @Test
    fun `a long-press on an armed modifier locks it rather than toggling it off`() {
        val locked = StickyModifiers().tapped(StickyKey.Ctrl).locked(StickyKey.Ctrl)
        assertEquals(StickyState.Locked, locked.ctrl)
    }

    @Test
    fun `consuming spends an armed modifier and keeps a locked one`() {
        val before = StickyModifiers(ctrl = StickyState.Armed, alt = StickyState.Locked)

        val after = before.consumed()

        assertEquals(StickyModifiers(ctrl = StickyState.Off, alt = StickyState.Locked), after)
        // Locked survives any number of keys.
        assertEquals(after, after.consumed().consumed())
    }

    @Test
    fun `held reports both armed and locked as held`() {
        assertEquals(HeldModifiers.NONE, StickyModifiers().held())
        assertEquals(HeldModifiers(ctrl = true), StickyModifiers(ctrl = StickyState.Armed).held())
        assertEquals(HeldModifiers(alt = true), StickyModifiers(alt = StickyState.Locked).held())
        assertEquals(
            HeldModifiers(ctrl = true, alt = true),
            StickyModifiers(ctrl = StickyState.Locked, alt = StickyState.Armed).held(),
        )
    }

    @Test
    fun `held modifiers know whether any is down`() {
        assertFalse(HeldModifiers.NONE.any)
        assertTrue(HeldModifiers(ctrl = true).any)
        assertTrue(HeldModifiers(alt = true).any)
    }

    @Test
    fun `the two modifiers are independent`() {
        val both = StickyModifiers().tapped(StickyKey.Ctrl).locked(StickyKey.Alt)
        assertEquals(StickyModifiers(ctrl = StickyState.Armed, alt = StickyState.Locked), both)

        // Disarming one leaves the other alone.
        assertEquals(StickyModifiers(alt = StickyState.Locked), both.tapped(StickyKey.Ctrl))
    }

    @Test
    fun `the sticky keys carry the left-hand evdev codes wprsd's xkb state reacts to`() {
        // Literals from /usr/include/linux/input-event-codes.h, not KeycodeMap,
        // so a wrong constant there would fail here rather than agree with itself.
        assertEquals(29, StickyKey.Ctrl.evdevCode)
        assertEquals(56, StickyKey.Alt.evdevCode)
    }
}
