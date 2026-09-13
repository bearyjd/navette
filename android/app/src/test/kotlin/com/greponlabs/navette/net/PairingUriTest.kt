package com.greponlabs.navette.net

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class PairingUriTest {
    @Test
    fun `parses a well formed pairing uri`() {
        val pairing = parsePairingUri("navette://pair?host=tower.ts.net&port=9417&token=ABCD1234ABCD1234ABCD1234")
        assertEquals(Pairing("tower.ts.net", 9417, "ABCD1234ABCD1234ABCD1234"), pairing)
    }

    @Test
    fun `rejects a uri that is not a navette pairing uri`() {
        // The scanner will happily read any QR code in the room.
        assertNull(parsePairingUri("https://example.com"))
        assertNull(parsePairingUri("navette://other?host=a&port=1&token=b"))
        assertNull(parsePairingUri("not a uri at all"))
    }

    @Test
    fun `rejects a uri missing any required field`() {
        assertNull(parsePairingUri("navette://pair?port=9417&token=ABCD1234ABCD1234ABCD1234"))
        assertNull(parsePairingUri("navette://pair?host=tower.ts.net&token=ABCD1234ABCD1234ABCD1234"))
        assertNull(parsePairingUri("navette://pair?host=tower.ts.net&port=9417"))
    }

    @Test
    fun `rejects a non numeric or out of range port`() {
        assertNull(parsePairingUri("navette://pair?host=t&port=abc&token=ABCD1234ABCD1234ABCD1234"))
        assertNull(parsePairingUri("navette://pair?host=t&port=99999&token=ABCD1234ABCD1234ABCD1234"))
    }

    @Test
    fun `rejects a port of exactly zero or exactly one past the max`() {
        assertNull(parsePairingUri("navette://pair?host=t&port=0&token=ABCD1234ABCD1234ABCD1234"))
        assertNull(parsePairingUri("navette://pair?host=t&port=65536&token=ABCD1234ABCD1234ABCD1234"))
    }

    @Test
    fun `accepts the port boundaries one and 65535`() {
        assertEquals(Pairing("t", 1, "ABCD1234ABCD1234ABCD1234"), parsePairingUri("navette://pair?host=t&port=1&token=ABCD1234ABCD1234ABCD1234"))
        assertEquals(Pairing("t", 65535, "ABCD1234ABCD1234ABCD1234"), parsePairingUri("navette://pair?host=t&port=65535&token=ABCD1234ABCD1234ABCD1234"))
    }

    @Test
    fun `rejects an empty field value`() {
        assertNull(parsePairingUri("navette://pair?host=&port=9417&token=ABCD1234ABCD1234ABCD1234"))
    }

    @Test
    fun `rejects a query with a pair that has no equals sign`() {
        assertNull(parsePairingUri("navette://pair?host=t&port&token=ABCD1234ABCD1234ABCD1234"))
    }

    @Test
    fun `rejects a query with a duplicate key`() {
        // Ambiguous input from a camera is refused, not guessed at -- the
        // last-write-wins collapse of a naive `.toMap()` would otherwise hand
        // back a Pairing that looks well-formed but reflects the wrong value.
        assertNull(parsePairingUri("navette://pair?host=a&host=b&port=9417&token=ABCD1234ABCD1234ABCD1234"))
    }

    @Test
    fun `parses an ipv6 literal host without percent decoding it`() {
        // Brackets and colons are legal, unencoded host characters on the
        // producing side (crates/navette-cli/src/main.rs `resolve_advertise_host`).
        val pairing =
            parsePairingUri("navette://pair?host=[fd7a:115c:a1e0::1]&port=9417&token=ABCD1234ABCD1234ABCD1234")
        assertEquals(Pairing("[fd7a:115c:a1e0::1]", 9417, "ABCD1234ABCD1234ABCD1234"), pairing)
    }
}

class PairingRedactionTest {
    @Test
    fun `toString never reveals the token`() {
        // Mirrors the Rust `secret_string_debug_never_reveals_the_value` test.
        // A data class's derived toString prints every field, and AppUiState
        // holds a Pairing -- so one `Log.d(TAG, "$state")` would put the
        // credential in logcat.
        val token = "ABCD1234ABCD1234ABCD1234"
        val rendered = Pairing("tower.ts.net", 9417, token).toString()
        assertFalse("toString leaked the token: $rendered", rendered.contains(token))
        assertTrue(rendered.contains("REDACTED"))
        // Host and port are not secrets, and a log line without them is useless
        // for diagnosing a connection.
        assertTrue(rendered.contains("tower.ts.net"))
        assertTrue(rendered.contains("9417"))
    }

    @Test
    fun `a container's toString does not leak it either`() {
        // The realistic leak is indirect: something else renders a field that
        // happens to hold a Pairing.
        val token = "ABCD1234ABCD1234ABCD1234"
        val rendered = listOf(Pairing("tower", 9417, token)).toString()
        assertFalse("a containing toString leaked the token: $rendered", rendered.contains(token))
    }
}
