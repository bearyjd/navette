package com.greponlabs.navette.ui.session

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class ClipboardBridgeTest {
    @Test
    fun `a local copy is forwarded`() {
        val bridge = ClipboardBridge()
        assertEquals("hello", bridge.onLocalClipboard("hello"))
    }

    @Test
    fun `a remote push is written locally`() {
        val bridge = ClipboardBridge()
        assertEquals("hello", bridge.onRemoteClipboard("hello"))
    }

    @Test
    fun `our own remote write is not forwarded back`() {
        val bridge = ClipboardBridge()
        bridge.onRemoteClipboard("hello")
        assertNull(
            "the listener firing on our own setPrimaryClip must not bounce back",
            bridge.onLocalClipboard("hello"),
        )
    }

    @Test
    fun `the echo token is one-shot`() {
        val bridge = ClipboardBridge()
        bridge.onRemoteClipboard("hello")
        assertNull(bridge.onLocalClipboard("hello"))
        assertEquals(
            "the same text copied again by the user must propagate",
            "hello",
            bridge.onLocalClipboard("hello"),
        )
    }

    @Test
    fun `an unchanged local clipboard is not resent`() {
        val bridge = ClipboardBridge()
        assertEquals("hello", bridge.onLocalClipboard("hello"))
        assertNull(
            "a resume read of unchanged content must not resend",
            bridge.onLocalClipboard("hello"),
        )
    }

    @Test
    fun `blank clipboard content is ignored`() {
        val bridge = ClipboardBridge()
        assertNull(bridge.onLocalClipboard(""))
    }

    @Test
    fun `over-cap text is not sent`() {
        val bridge = ClipboardBridge()
        assertNull(bridge.onLocalClipboard("a".repeat(MAX_CLIPBOARD_BYTES + 1)))
    }
}
