package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.ConnectionState

/**
 * How many rebuilds a dropped media socket gets before the screen stops
 * retrying on its own and offers a manual reconnect. The server replays codec
 * config and the latest keyframe to every fresh attachment
 * (`crates/navetted/src/media.rs`'s `attach`, and its
 * `reconnect_starts_with_config_and_latest_keyframe` test), so a retry that
 * lands while the session is still alive resumes the picture unaided.
 */
const val MAX_RECONNECT_ATTEMPTS: Int = 5

/**
 * Base delay between retries, multiplied by the attempt number for a linear
 * backoff (1s, 2s, ...). Short enough that a tailnet blip recovers before the
 * user reaches for the phone, bounded so a dead host is given up on in
 * seconds rather than hammered.
 */
const val RECONNECT_BASE_DELAY_MS: Long = 1_000L

/**
 * The retry policy for a dropped media socket, as pure functions over plain
 * values so every decision is unit-tested rather than trusted. The session
 * screen owns the counters and the clock; this owns the rules.
 */
object ReconnectPolicy {
    /**
     * A socket that has failed, closed, or been refused, as opposed to one
     * still connecting or live. [ConnectionState.Unauthorized] counts as
     * dropped here -- it is a terminal failure like [ConnectionState.Failed],
     * just one [shouldRetry] refuses to retry -- so the session screen's
     * retry effect still observes the transition instead of waiting forever
     * on a drop that never satisfies its predicate.
     */
    fun isDropped(connection: ConnectionState): Boolean =
        connection is ConnectionState.Failed ||
            connection is ConnectionState.Disconnected ||
            connection is ConnectionState.Unauthorized

    /**
     * Whether a drop should be retried automatically after [attempt]
     * retries have already been spent on it.
     *
     * A closed guest window ([streamEnded]) and a decoder failure
     * ([decodeError]) are terminal: rebuilding the socket cannot bring back a
     * window that is gone, and a stream the decoder cannot handle will fail
     * the same way again. Both leave the manual reconnect available.
     *
     * [unauthorized] is terminal too, and checked first: a rotated token
     * otherwise produces an invisible infinite reconnect loop -- the phone
     * spinning forever while the daemon refuses every attempt, which the
     * user reads as a network problem rather than what it is. Retrying
     * cannot succeed until the user pairs again.
     */
    fun shouldRetry(attempt: Int, streamEnded: Boolean, decodeError: String?, unauthorized: Boolean): Boolean {
        if (unauthorized) return false
        return !streamEnded && decodeError == null && attempt < MAX_RECONNECT_ATTEMPTS
    }

    /** How long to wait before retry number [attempt], counting from one. */
    fun delayMs(attempt: Int): Long = RECONNECT_BASE_DELAY_MS * attempt.coerceAtLeast(1)
}
