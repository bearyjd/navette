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
    /** A socket that has failed or closed, as opposed to one still connecting or live. */
    fun isDropped(connection: ConnectionState): Boolean =
        connection is ConnectionState.Failed || connection is ConnectionState.Disconnected

    /**
     * Whether a drop should be retried automatically after [attemptsUsed]
     * retries have already been spent on it.
     *
     * A closed guest window ([streamEnded]) and a decoder failure
     * ([decodeError]) are terminal: rebuilding the socket cannot bring back a
     * window that is gone, and a stream the decoder cannot handle will fail
     * the same way again. Both leave the manual reconnect available.
     */
    fun shouldRetry(attemptsUsed: Int, streamEnded: Boolean, decodeError: String?): Boolean =
        !streamEnded && decodeError == null && attemptsUsed < MAX_RECONNECT_ATTEMPTS

    /** How long to wait before retry number [attempt], counting from one. */
    fun delayMs(attempt: Int): Long = RECONNECT_BASE_DELAY_MS * attempt.coerceAtLeast(1)
}
