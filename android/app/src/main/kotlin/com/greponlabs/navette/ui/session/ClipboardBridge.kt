package com.greponlabs.navette.ui.session

/** Mirrors the daemon's cap. UTF-8 bytes, not characters. */
const val MAX_CLIPBOARD_BYTES: Int = 1024 * 1024

/**
 * Decides what to do with a clipboard change, and touches no Android
 * framework class -- which is what makes every decision here a plain JVM
 * test. [SessionScreen] owns the `ClipboardManager` calls.
 *
 * Clipboard content is never logged.
 */
class ClipboardBridge {
    /**
     * The last value we wrote locally, suppressing the listener firing on
     * our own write. One-shot: cleared on first match, so the same text
     * genuinely copied again still propagates. A retained token would
     * silently drop every later genuine copy of the same text.
     */
    private var echoFromLocal: String? = null

    /**
     * The last value we sent, so a resume read of unchanged content does
     * not resend it.
     */
    private var lastSent: String? = null

    /** A local clipboard change. Returns the text to send, or null. */
    fun onLocalClipboard(text: String): String? {
        if (text.isEmpty()) return null
        if (text.toByteArray(Charsets.UTF_8).size > MAX_CLIPBOARD_BYTES) return null

        if (echoFromLocal == text) {
            echoFromLocal = null
            return null
        }
        if (lastSent == text) return null

        lastSent = text
        return text
    }

    /** A push from the daemon. Returns the text to write locally, or null. */
    fun onRemoteClipboard(text: String): String? {
        if (text.isEmpty()) return null
        // Deliberately does NOT touch lastSent. Setting it here would make a
        // genuine later local copy of this same text look like an unchanged
        // resend and get dropped after the one-shot echo token above is
        // already spent -- exactly the failure the one-shot test below
        // exists to catch.
        echoFromLocal = text
        return text
    }
}
