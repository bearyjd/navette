package com.greponlabs.navette.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ControlReconnectPolicyTest {
    @Test
    fun `delay doubles from one second and caps at thirty`() {
        assertEquals(
            listOf(1_000L, 2_000L, 4_000L, 8_000L, 16_000L, 30_000L, 30_000L),
            (1..7).map(ControlReconnectPolicy::delayMs),
        )
    }

    @Test
    fun `delay is safe for out-of-range attempts`() {
        assertEquals(1_000L, ControlReconnectPolicy.delayMs(0))
        assertEquals(30_000L, ControlReconnectPolicy.delayMs(Int.MAX_VALUE))
    }

    @Test
    fun `retries exactly five times`() {
        assertTrue((1..5).all(ControlReconnectPolicy::shouldRetry))
        assertFalse(ControlReconnectPolicy.shouldRetry(6))
        assertFalse(ControlReconnectPolicy.shouldRetry(0))
    }

    @Test
    fun `notifies from the second failure`() {
        assertFalse(ControlReconnectPolicy.shouldNotify(1))
        assertTrue(ControlReconnectPolicy.shouldNotify(2))
        assertTrue(ControlReconnectPolicy.shouldNotify(3))
    }
}
