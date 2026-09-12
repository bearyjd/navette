package com.greponlabs.navette.net

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
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
    fun `parses an ipv6 literal host without percent decoding it`() {
        // Brackets and colons are legal, unencoded host characters on the
        // producing side (crates/navette-cli/src/main.rs `resolve_advertise_host`).
        val pairing =
            parsePairingUri("navette://pair?host=[fd7a:115c:a1e0::1]&port=9417&token=ABCD1234ABCD1234ABCD1234")
        assertEquals(Pairing("[fd7a:115c:a1e0::1]", 9417, "ABCD1234ABCD1234ABCD1234"), pairing)
    }
}
