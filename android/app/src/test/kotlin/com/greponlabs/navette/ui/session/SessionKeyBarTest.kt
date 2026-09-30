package com.greponlabs.navette.ui.session

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class SessionKeyBarTest {
    @Test
    fun `every chip state is spoken, and only Off reads as unselected`() {
        // TalkBack reads the label, then this: without it an armed or locked
        // Ctrl was indistinguishable from an idle one.
        assertEquals("Off", StickyState.Off.spoken)
        assertEquals("Armed for the next key", StickyState.Armed.spoken)
        assertEquals("Locked", StickyState.Locked.spoken)
        assertFalse(StickyState.Off.engaged)
        assertTrue(StickyState.Armed.engaged)
        assertTrue(StickyState.Locked.engaged)
    }
}
