package com.greponlabs.navette.ui

/** Automatic retries of a failed control channel before giving up. */
const val MAX_CONTROL_RECONNECT_ATTEMPTS: Int = 5

private const val CONTROL_BASE_DELAY_MS: Long = 1_000L
private const val CONTROL_MAX_DELAY_MS: Long = 30_000L

/** The failure count (1-based) at which the user is first told about it. */
private const val SNACKBAR_AFTER_FAILURES: Int = 2

/**
 * Retry rules for a dropped control channel, as pure functions so each
 * decision is unit-tested. [AppViewModel] owns the counters and the clock.
 * Unlike the media path's linear backoff (`ReconnectPolicy`), this one is
 * exponential: 1s, 2s, 4s, 8s, 16s, capped at 30s.
 */
object ControlReconnectPolicy {
    /** Whether to retry after [failures] consecutive failures (counting from one). */
    fun shouldRetry(failures: Int): Boolean = failures in 1..MAX_CONTROL_RECONNECT_ATTEMPTS

    /** Delay before retry number [attempt], counting from one. */
    fun delayMs(attempt: Int): Long {
        val exponent = (attempt.coerceAtLeast(1) - 1).coerceAtMost(20)
        return (CONTROL_BASE_DELAY_MS shl exponent).coerceAtMost(CONTROL_MAX_DELAY_MS)
    }

    /** Whether the [failures]th consecutive failure should surface a snackbar. */
    fun shouldNotify(failures: Int): Boolean = failures >= SNACKBAR_AFTER_FAILURES
}
