package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.MediaInput
import com.greponlabs.navette.net.mediaJson

/**
 * Mirrors the daemon's *graceful* per-message limit -- `MAX_INPUT_MESSAGE`
 * in `crates/navette-protocol/src/media.rs:10` -- not the harder cap the
 * WebSocket transport itself enforces (`api.rs:127`,
 * `MAX_INPUT_MESSAGE * 2` = 32 KiB). Above that harder cap, tungstenite
 * fails the frame on the read path before the daemon's own graceful
 * "input message exceeds..." handler ever runs, tearing the whole media
 * socket down -- confirmed on-device: video drops, the client reconnects.
 * Staying at or under this 16 KiB figure keeps every clipboard send inside
 * the band the daemon refuses cleanly instead.
 */
const val MAX_INPUT_MESSAGE_BYTES: Int = 16 * 1024

/**
 * The size that actually crosses the wire for [text], not its raw UTF-8
 * byte count. JSON escaping inflates quotes and backslashes 2x and control
 * characters 6x (`\uXXXX`), so a 6 KiB clipboard of the wrong bytes could
 * still clear [MAX_INPUT_MESSAGE_BYTES] under a raw-length guard -- this
 * measures the actual encoded frame instead, the same encoding
 * `MediaClient.sendInput` performs when it actually sends.
 */
private fun encodedClipboardFrameBytes(text: String): Int =
    mediaJson.encodeToString(MediaInput.serializer(), MediaInput.SetClipboard(text)).toByteArray(Charsets.UTF_8).size

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
     * The last value actually confirmed sent, so a resume read of unchanged
     * content does not resend it. Set only by [markSent], once delivery is
     * confirmed -- never by the decision methods themselves, which merely
     * decide a send is warranted and may still fail or be retried. Cleared
     * by [onRemoteClipboard]: once the daemon pushes something here, this
     * value no longer describes what the phone's clipboard holds, so
     * comparing a later local read against it is meaningless -- see that
     * method for the regression a stale value here caused.
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
        if (encodedClipboardFrameBytes(text) > MAX_INPUT_MESSAGE_BYTES) return null

        if (echoFromLocal == text) {
            echoFromLocal = null
            return null
        }
        if (lastSent == text) return null

        // A genuine send is being decided here, not merely attempted: the
        // phone's clipboard has moved past whatever the daemon last pushed
        // (this text is neither that echo nor the last confirmed send), so
        // a later resume must stop comparing against that stale push. Left
        // set, [onLocalClipboardResume] would keep this exact text from
        // ever reaching the guest again once the phone's clipboard cycles
        // back around to it after an intervening remote push moved
        // `phone_text` on beyond it.
        lastRemote = null
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

    /**
     * Commits [text] as sent. Called only once delivery is actually
     * confirmed -- not from [onLocalClipboard]/[onLocalClipboardResume]
     * themselves, which only *decide* to send.
     *
     * Deciding and confirming are different moments precisely because a
     * decision can fail to reach the socket and be retried elsewhere (see
     * `SessionController.sendClipboardOrRetryOnConnect`). Writing [lastSent]
     * at decision time -- the original shape of this method -- made a
     * dropped-then-retried attempt indistinguishable from a delivered one:
     * if the retry itself then failed (`close()` cancelling it mid-wait, for
     * instance), [lastSent] already held this text, so the very next resume
     * reading the same still-undelivered clipboard would see
     * `lastSent == text`, decide there was nothing to send, and that text
     * would never reach the guest for the rest of the session -- silently,
     * and only on a reconnect that itself failed, which is exactly when a
     * retry exists to matter.
     */
    fun markSent(text: String) {
        lastSent = text
    }

    /** A push from the daemon. Returns the text to write locally, or null. */
    fun onRemoteClipboard(text: String): String? {
        if (text.isEmpty()) return null
        // Deliberately does NOT SET lastSent to this text. Doing that would
        // make a genuine later local copy of this same text look like an
        // unchanged resend and get dropped after the one-shot echo token
        // above is already spent -- exactly the failure the one-shot test
        // below exists to catch.
        //
        // It IS cleared, though: a controller review caught the mirror bug
        // this asymmetry left behind. Trace: phone sends A (markSent sets
        // lastSent = A); guest copies B; this method fires, setting
        // echoFromLocal and lastRemote to B but leaving lastSent = A
        // untouched; the write of B to the system clipboard fires the
        // listener, which consumes the echo and returns before ever
        // touching lastSent. The user then genuinely re-copies A:
        // echoFromLocal is spent, but lastSent still equals A, so
        // onLocalClipboard's `lastSent == text` check silently drops it --
        // a real, later copy discarded because of a send from before an
        // intervening remote push. A remote push means the phone's
        // clipboard no longer holds whatever we last sent, so lastSent's
        // value is meaningless from here on; clearing it is what keeps a
        // later genuine copy of that same text from being mistaken for an
        // unchanged resend.
        lastSent = null
        echoFromLocal = text
        lastRemote = text
        return text
    }

    /** A remote image replaces any text value this bridge remembered. */
    fun onRemoteClipboardBlob() {
        lastSent = null
        lastRemote = null
        echoFromLocal = null
    }
}
