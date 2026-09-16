package com.greponlabs.navette.ui.session

import org.junit.Assert.assertEquals
import org.junit.Test

class ClipboardResumeTest {
    @Test
    fun resumeOnlyForwardsAnActualTextItemNotAnImageUri() {
        assertEquals("copied text", resumeClipboardText("copied text"))
        assertEquals(null, resumeClipboardText(null))
    }

    @Test
    fun localImageClaimsOrderBeforeASlowMimeLookupCanObserveANewerRemoteEvent() {
        val events = mutableListOf<String>()
        var generation = 0L
        val claimed =
            claimClipboardImageBeforeMime(
                "content://outside/image",
                {
                    events += "local-claim"
                    ++generation
                },
            ) {
                events += "mime-lookup"
                ++generation // A newer remote clipboard event arrives while the provider is slow.
                "image/png"
            }

        assertEquals(listOf("local-claim", "mime-lookup"), events)
        assertEquals(1L, claimed?.first)
        assertEquals(2L, generation)
    }
}
