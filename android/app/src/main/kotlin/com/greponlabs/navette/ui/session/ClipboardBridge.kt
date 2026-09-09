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

    /**
     * The last value the daemon pushed here, kept even after [echoFromLocal]
     * is spent. This is what [onLocalClipboardResume] checks that
     * [onLocalClipboard] does not: on a reconnect, [SessionScreen] rebuilds
     * its socket and the lifecycle observer re-syncs to the current state,
     * delivering an immediate `ON_RESUME` -- but this bridge, unlike the
     * controller, survives the reconnect (it is `remember`ed by session, not
     * by reconnect attempt). Without this field that resume would read the
     * phone's clipboard -- still holding the daemon's last push, since
     * nothing local changed -- find the one-shot echo token already
     * consumed by the listener's earlier fire, and forward it right back as
     * if it were new. If the guest copied something else while the socket
     * was down, the daemon would receive this stale value after the guest's
     * newer one and revert it. `lastSent` cannot substitute for this: it is
     * cleared by the very consumption this field needs to survive.
     */
    private var lastRemote: String? = null

    /** A local clipboard change from the listener firing. Returns the text to send, or null. */
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

    /**
     * A local clipboard read on `ON_RESUME`. Same decisions as
     * [onLocalClipboard], plus a check the listener path does not need: text
     * matching the daemon's last push is never forwarded from here, even once
     * the one-shot echo token has already been consumed by an earlier
     * listener fire. See [lastRemote] for why a resume is not just another
     * listener fire.
     */
    fun onLocalClipboardResume(text: String): String? {
        if (text == lastRemote) return null
        return onLocalClipboard(text)
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
        lastRemote = text
        return text
    }
}
