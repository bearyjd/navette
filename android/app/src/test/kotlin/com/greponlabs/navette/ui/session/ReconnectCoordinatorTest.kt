package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.ConnectionState
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class ReconnectCoordinatorTest {
    @Test
    fun `a dropped socket emits one rebuild only after its policy delay`() = runTest {
        val state = MutableStateFlow(SessionUiState())
        val scheduledAttempts = mutableListOf<Int>()
        var rebuilt = false

        val job =
            launch {
                rebuilt = awaitReconnectRebuild(state, attemptsUsed = 0) { scheduledAttempts += it }
            }
        runCurrent()
        state.value = state.value.copy(connection = ConnectionState.Failed("tailnet route lost"))
        runCurrent()

        assertEquals(listOf(1), scheduledAttempts)
        assertFalse(rebuilt)
        advanceTimeBy(RECONNECT_BASE_DELAY_MS - 1)
        runCurrent()
        assertFalse(rebuilt)

        advanceTimeBy(1)
        runCurrent()
        assertTrue(rebuilt)
        assertTrue(job.isCompleted)
    }

    @Test
    fun `an unauthorized socket emits no rebuild signal`() = runTest {
        val state = MutableStateFlow(SessionUiState(connection = ConnectionState.Unauthorized))
        val scheduledAttempts = mutableListOf<Int>()

        val rebuilt = awaitReconnectRebuild(state, attemptsUsed = 0) { scheduledAttempts += it }

        assertFalse(rebuilt)
        assertEquals(emptyList<Int>(), scheduledAttempts)
    }
}
