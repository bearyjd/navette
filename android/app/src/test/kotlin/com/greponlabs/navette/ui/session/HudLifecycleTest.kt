package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.ConnectionState
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class HudLifecycleTest {
    @Test
    fun `HUD worker stops immediately when its socket drops`() =
        runTest {
            val connection = MutableStateFlow<ConnectionState>(ConnectionState.Connecting)
            var ticks = 0

            backgroundScope.launchHudWhileConnected(connection) {
                while (true) {
                    ticks += 1
                    delay(1_000L)
                }
            }
            runCurrent()
            assertEquals("the HUD must not ping before the socket opens", 0, ticks)

            connection.value = ConnectionState.Connected
            runCurrent()
            assertEquals(1, ticks)

            advanceTimeBy(1_000L)
            runCurrent()
            assertEquals(2, ticks)

            connection.value = ConnectionState.Failed("tailnet route lost")
            runCurrent()
            advanceTimeBy(10_000L)
            runCurrent()
            assertEquals("a terminal overlay must not keep a HUD worker alive", 2, ticks)
        }
}
