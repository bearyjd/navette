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

    @Test
    fun `media URL carries the session name in its path`() {
        assertEquals(
            "ws://tower:9417/v1/sessions/work/media",
            mediaWebSocketUrl("tower", "work"),
        )
    }

    @Test
    fun `media URL applies the same IPv6 bracketing as the control URL`() {
        assertEquals(
            "ws://[fd7a:115c:a1e0::1]:9417/v1/sessions/work/media",
            mediaWebSocketUrl("fd7a:115c:a1e0::1", "work"),
        )
        assertEquals(
            "ws://192.168.1.5:9417/v1/sessions/work/media",
            mediaWebSocketUrl("192.168.1.5", "work"),
        )
        assertEquals(
            "ws://[fd7a:115c:a1e0::1]:9417/v1/sessions/work/media",
            mediaWebSocketUrl("[fd7a:115c:a1e0::1]", "work"),
        )
    }

    @Test
    fun `media URL honors a custom port and the full legal session-name alphabet`() {
        // navetted validates names to [a-z0-9_-]{1,64} (registry.rs's
        // validate_session_name), so this is the widest shape that can
        // actually reach mediaWebSocketUrl -- and none of it needs escaping.
        assertEquals(
            "ws://tower:1234/v1/sessions/my_session-2/media",
            mediaWebSocketUrl("tower", "my_session-2", port = 1234),
        )
    }

    @Test
    fun `file collection uses HTTP and replaces only the media route`() {
        assertEquals(
            "http://[fd7a:115c:a1e0::1]:9417/v1/sessions/work/files",
            fileTransferCollectionUrl("ws://[fd7a:115c:a1e0::1]:9417/v1/sessions/work/media"),
        )
    }
}
