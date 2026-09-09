package com.greponlabs.navette.ui.session

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
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

    @Test
    fun `the cap is UTF-8 bytes, not characters`() {
        // A 3-byte character: MAX/3 + 1 of them is over the cap in bytes
        // while being far under it in char count -- a length-based guard
        // would wrongly accept this.
        val text = "☃".repeat(MAX_CLIPBOARD_BYTES / 3 + 1)
        assertTrue("under the cap by char count, over it by bytes", text.length < MAX_CLIPBOARD_BYTES)
        assertNull(ClipboardBridge().onLocalClipboard(text))
    }

    @Test
    fun `text exactly at the cap is forwarded`() {
        val text = "a".repeat(MAX_CLIPBOARD_BYTES)
        assertEquals(text, ClipboardBridge().onLocalClipboard(text))
    }

    @Test
    fun `a resume after reconnect does not re-forward the daemon's own push`() {
        val bridge = ClipboardBridge()
        bridge.onRemoteClipboard("hello")
        // The listener consumes the one-shot echo token, as it would on the
        // original setPrimaryClip -- exactly as it does before any reconnect.
        assertNull(bridge.onLocalClipboard("hello"))
        // A reconnect rebuilds the SessionController and its socket, but not
        // this bridge (remembered by session, not by reconnect attempt). Its
        // freshly re-added LifecycleEventObserver syncs to the current
        // RESUMED state immediately, delivering an ON_RESUME that reads the
        // same, still-unchanged clipboard text. That must not re-forward the
        // daemon's own value as if it were new.
        assertNull(
            "a resume must not re-forward what the daemon itself just pushed here",
            bridge.onLocalClipboardResume("hello"),
        )
    }

    @Test
    fun `a resume still forwards a genuinely new local copy`() {
        val bridge = ClipboardBridge()
        assertEquals("hello", bridge.onLocalClipboardResume("hello"))
    }
}
