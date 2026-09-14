package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.ConnectionState
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first

/**
 * Waits for one terminal media state and, when policy permits, schedules and
 * delays exactly one controller rebuild.
 *
 * The scheduling callback deliberately runs before the delay. The screen has
 * always charged its retry budget while the reconnect overlay is visible, so
 * a recomposition during the backoff sees the same attempt count as before
 * this coordinator was extracted.
 */
internal suspend fun awaitReconnectRebuild(
    state: StateFlow<SessionUiState>,
    attemptsUsed: Int,
    onRetryScheduled: (nextAttempt: Int) -> Unit,
): Boolean {
    val dropped =
        state.first {
            ReconnectPolicy.isDropped(it.connection) || it.streamEnded || it.decodeError != null
        }
    if (
        !ReconnectPolicy.shouldRetry(
            attemptsUsed,
            dropped.streamEnded,
            dropped.decodeError,
            dropped.connection is ConnectionState.Unauthorized,
        )
    ) {
        return false
    }

    val nextAttempt = attemptsUsed + 1
    onRetryScheduled(nextAttempt)
    delay(ReconnectPolicy.delayMs(nextAttempt))
    return true
}
