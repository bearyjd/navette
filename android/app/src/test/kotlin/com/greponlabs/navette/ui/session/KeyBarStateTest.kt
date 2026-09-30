package com.greponlabs.navette.ui.session

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class KeyBarStateTest {
    private val states =
        listOf(
            KeyBarState(),
            KeyBarState(pinned = true),
            KeyBarState(dismissed = true),
            KeyBarState(pinned = true, dismissed = true),
        )

    /**
     * The defect this guards: the Keys button used to toggle only `pinned`,
     * so with the keyboard up the bar was already showing and the tap changed
     * nothing while the label flipped to "Hide keys".
     */
    @Test
    fun `a tap always changes what is on screen`() {
        for (state in states) {
            for (imeRaised in listOf(false, true)) {
                val before = state.visible(imeRaised)
                val after = state.toggled(imeRaised).visible(imeRaised)
                assertEquals("$state at imeRaised=$imeRaised did not flip", !before, after)
            }
        }
    }

    @Test
    fun `the bar comes up with the keyboard`() {
        assertTrue(KeyBarState().visible(imeRaised = true))
        assertFalse(KeyBarState().visible(imeRaised = false))
    }

    @Test
    fun `the keyboard can be used without the bar`() {
        val dismissed = KeyBarState().toggled(imeRaised = true)

        assertFalse(dismissed.visible(imeRaised = true))
        // The dismissal is remembered for the session, so raising the
        // keyboard again does not bring the bar back uninvited.
        assertFalse(dismissed.visible(imeRaised = true))
        assertTrue("one tap brings it back", dismissed.toggled(imeRaised = true).visible(imeRaised = true))
    }

    @Test
    fun `the bar can be pinned without the keyboard and follows the keyboard down`() {
        val pinned = KeyBarState().toggled(imeRaised = false)
        assertTrue(pinned.visible(imeRaised = false))
        assertTrue(pinned.visible(imeRaised = true))

        // Shown only because the keyboard is up: hiding the keyboard hides it.
        assertFalse(KeyBarState().visible(imeRaised = false))
    }

    @Test
    fun `hiding the bar while it is pinned unpins it rather than leaving it on screen`() {
        val hidden = KeyBarState(pinned = true).toggled(imeRaised = false)

        assertFalse(hidden.visible(imeRaised = false))
        assertFalse("and it stays hidden when the keyboard comes up", hidden.visible(imeRaised = true))
    }
}
