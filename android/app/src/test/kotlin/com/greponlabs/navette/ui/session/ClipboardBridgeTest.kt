package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.MediaInput
import com.greponlabs.navette.net.mediaJson
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** What [MediaClient.sendInput] would actually put on the wire for [text]. */
private fun encodedFrameBytes(text: String): Int =
    mediaJson.encodeToString(MediaInput.serializer(), MediaInput.SetClipboard(text)).toByteArray(Charsets.UTF_8).size

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
    fun `an unchanged local clipboard is not resent, once the first send is confirmed`() {
        val bridge = ClipboardBridge()
        assertEquals("hello", bridge.onLocalClipboard("hello"))
        bridge.markSent("hello")
        assertNull(
            "a resume read of unchanged content must not resend",
            bridge.onLocalClipboard("hello"),
        )
    }

    /**
     * The regression a controller review caught: the original `onLocalClipboard`
     * wrote `lastSent` at decision time, so a decision that never reached the
     * socket (dropped, then retried by `SessionController`) already looked
     * sent. If that retry itself then failed, the text was gone for the rest
     * of the session -- the very next resume reading the same still-undelivered
     * clipboard would see `lastSent == text` and decide there was nothing to
     * do. `markSent` must be a separate step the caller takes only once
     * delivery is confirmed, not a side effect of deciding to send.
     */
    @Test
    fun `a decision that is never confirmed sent is retried on the next resume`() {
        val bridge = ClipboardBridge()
        assertEquals("hello", bridge.onLocalClipboard("hello"))
        // No markSent call: the send was decided but never confirmed --
        // exactly the state after a dropped send whose retry also failed.
        assertEquals(
            "an unconfirmed decision must not have poisoned lastSent",
            "hello",
            bridge.onLocalClipboardResume("hello"),
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
        // Comfortably over MAX_INPUT_MESSAGE_BYTES even after JSON overhead.
        assertNull(bridge.onLocalClipboard("a".repeat(MAX_INPUT_MESSAGE_BYTES + 1)))
    }

    @Test
    fun `the cap is on the encoded frame, not the raw text`() {
        // Quotes double under JSON escaping (" -> \"): raw text bytes stay
        // under the cap while the frame that would actually cross the wire
        // does not. A raw-length guard would look correct here and be
        // wrong -- exactly what a controller review caught in the original
        // MAX_CLIPBOARD_BYTES check, which measured raw UTF-8 bytes against
        // a limit the transport does not honor at that size anyway.
        val text = "\"".repeat(MAX_INPUT_MESSAGE_BYTES - 100)
        assertTrue("under the cap by raw byte count", text.toByteArray(Charsets.UTF_8).size < MAX_INPUT_MESSAGE_BYTES)
        assertTrue("but over the cap once JSON-encoded", encodedFrameBytes(text) > MAX_INPUT_MESSAGE_BYTES)
        assertNull(ClipboardBridge().onLocalClipboard(text))
    }

    @Test
    fun `text whose encoded frame is exactly at the cap is forwarded`() {
        val overhead = encodedFrameBytes("")
        val text = "a".repeat(MAX_INPUT_MESSAGE_BYTES - overhead)
        assertEquals(MAX_INPUT_MESSAGE_BYTES, encodedFrameBytes(text))
        assertEquals(text, ClipboardBridge().onLocalClipboard(text))
    }

    @Test
    fun `text one byte over the encoded cap is refused`() {
        val overhead = encodedFrameBytes("")
        val text = "a".repeat(MAX_INPUT_MESSAGE_BYTES - overhead + 1)
        assertEquals(MAX_INPUT_MESSAGE_BYTES + 1, encodedFrameBytes(text))
        assertNull(ClipboardBridge().onLocalClipboard(text))
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

    /**
     * The regression a controller review caught: left set forever,
     * [lastRemote] would keep this exact text from ever reaching the guest
     * again via resume, even long after an intervening phone copy moved the
     * daemon's own `phone_text` on past it -- at which point a guest paste
     * should get "A" again, not the stale value the resume path silently
     * refused to re-send.
     */
    @Test
    fun `a resume forwards a remote value again once an intervening send has moved past it`() {
        val bridge = ClipboardBridge()
        bridge.onRemoteClipboard("A")
        // The push's own setPrimaryClip fires the listener once, consuming
        // the echo token exactly as it would in real use.
        assertNull(bridge.onLocalClipboard("A"))
        // Immediately after, a resume must not re-forward the daemon's own
        // value -- this is the existing, still-correct suppression.
        assertNull(bridge.onLocalClipboardResume("A"))

        // The user genuinely copies something else; SessionController
        // would confirm delivery with markSent once client.sendInput
        // succeeds.
        assertEquals("B", bridge.onLocalClipboard("B"))
        bridge.markSent("B")

        // The user copies "A" again for real. Read via a resume -- the path
        // most real transfers take -- this must reach the guest, not be
        // silently dropped because it happens to equal the daemon's push
        // from long before.
        assertEquals(
            "a remote value copied again after an intervening send must reach the guest",
            "A",
            bridge.onLocalClipboardResume("A"),
        )
    }
}
