package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.ConnectionState
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ReconnectPolicyTest {
    @Test
    fun `only a failed or closed socket counts as dropped`() {
        assertTrue(ReconnectPolicy.isDropped(ConnectionState.Failed("gone")))
        assertTrue(ReconnectPolicy.isDropped(ConnectionState.Disconnected))
        assertFalse(ReconnectPolicy.isDropped(ConnectionState.Connecting))
        assertFalse(ReconnectPolicy.isDropped(ConnectionState.Connected))
    }

    @Test
    fun `retries until the budget is spent and not once more`() {
        for (used in 0 until MAX_RECONNECT_ATTEMPTS) {
            assertTrue("attempt $used", ReconnectPolicy.shouldRetry(used, streamEnded = false, decodeError = null))
        }
        assertFalse(ReconnectPolicy.shouldRetry(MAX_RECONNECT_ATTEMPTS, streamEnded = false, decodeError = null))
        assertFalse(ReconnectPolicy.shouldRetry(MAX_RECONNECT_ATTEMPTS + 1, streamEnded = false, decodeError = null))
    }

    @Test
    fun `a closed guest window is terminal`() {
        assertFalse(ReconnectPolicy.shouldRetry(0, streamEnded = true, decodeError = null))
    }

    @Test
    fun `a decoder failure is terminal`() {
        assertFalse(ReconnectPolicy.shouldRetry(0, streamEnded = false, decodeError = "codec error 0x1"))
    }

    @Test
    fun `backoff is linear in the attempt number`() {
        assertEquals(RECONNECT_BASE_DELAY_MS, ReconnectPolicy.delayMs(1))
        assertEquals(RECONNECT_BASE_DELAY_MS * 3, ReconnectPolicy.delayMs(3))
        assertEquals(RECONNECT_BASE_DELAY_MS * MAX_RECONNECT_ATTEMPTS, ReconnectPolicy.delayMs(MAX_RECONNECT_ATTEMPTS))
    }

    @Test
    fun `a nonsensical attempt number still waits at least one base delay`() {
        assertEquals(RECONNECT_BASE_DELAY_MS, ReconnectPolicy.delayMs(0))
        assertEquals(RECONNECT_BASE_DELAY_MS, ReconnectPolicy.delayMs(-4))
    }
}
