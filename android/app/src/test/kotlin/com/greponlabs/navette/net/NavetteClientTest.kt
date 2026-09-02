package com.greponlabs.navette.net

import com.greponlabs.navette.protocol.CONTROL_WEBSOCKET_PATH
import org.junit.Assert.assertEquals
import org.junit.Test

class NavetteClientTest {
    @Test
    fun `hostname passes through unchanged`() {
        assertEquals(
            "ws://tower:9417$CONTROL_WEBSOCKET_PATH",
            controlWebSocketUrl("tower"),
        )
    }

    @Test
    fun `IPv4 address passes through unchanged`() {
        assertEquals(
            "ws://192.168.1.5:9417$CONTROL_WEBSOCKET_PATH",
            controlWebSocketUrl("192.168.1.5"),
        )
    }

    @Test
    fun `bare Tailscale IPv6 address is bracket-wrapped`() {
        assertEquals(
            "ws://[fd7a:115c:a1e0::1]:9417$CONTROL_WEBSOCKET_PATH",
            controlWebSocketUrl("fd7a:115c:a1e0::1"),
        )
    }

    @Test
    fun `already-bracketed IPv6 address is left as-is`() {
        assertEquals(
            "ws://[fd7a:115c:a1e0::1]:9417$CONTROL_WEBSOCKET_PATH",
            controlWebSocketUrl("[fd7a:115c:a1e0::1]"),
        )
    }

    @Test
    fun `a host string with a single colon is not mistaken for IPv6`() {
        // Not a valid authority once :9417 is appended again, but that's a
        // malformed-input case NavetteClient#connect guards against rather
        // than something this pure formatter can fully disambiguate.
        assertEquals(
            "ws://tower:9417:9417$CONTROL_WEBSOCKET_PATH",
            controlWebSocketUrl("tower:9417"),
        )
    }

    @Test
    fun `custom port is honored`() {
        assertEquals(
            "ws://tower:1234$CONTROL_WEBSOCKET_PATH",
            controlWebSocketUrl("tower", port = 1234),
        )
    }
}
