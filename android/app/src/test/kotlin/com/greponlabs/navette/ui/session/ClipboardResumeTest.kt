package com.greponlabs.navette.ui.session

import org.junit.Assert.assertEquals
import org.junit.Test

class ClipboardResumeTest {
    @Test
    fun resumeOnlyForwardsAnActualTextItemNotAnImageUri() {
        assertEquals("copied text", resumeClipboardText("copied text"))
        assertEquals(null, resumeClipboardText(null))
    }
}
