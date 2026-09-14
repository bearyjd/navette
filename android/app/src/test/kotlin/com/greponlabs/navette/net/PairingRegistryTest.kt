package com.greponlabs.navette.net

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class PairingRegistryTest {
    private val token = "ABCD1234ABCD1234ABCD1234"

    @Test
    fun `codec round trips normalized active registry without leaking token`() {
        val registry = PairingRegistryCodec.upsert(PairingRegistry(), Pairing("Tower.TS.Net", 9417, token.lowercase()))
        val decoded = PairingRegistryCodec.decode(PairingRegistryCodec.encode(registry)) as RegistryDecode.Valid
        assertEquals("tower.ts.net", decoded.registry.active!!.pairing.host)
        assertEquals(token, decoded.registry.active!!.pairing.token)
        assertFalse(decoded.registry.active.toString().contains(token))
    }

    @Test
    fun `re-pairing normalized endpoint replaces token rather than duplicating host`() {
        val first = PairingRegistryCodec.upsert(PairingRegistry(), Pairing("Tower", 9417, token))
        val second = PairingRegistryCodec.upsert(first, Pairing("tower", 9417, "ZZZZ1234ZZZZ1234ZZZZ1234"))
        assertEquals(1, second.hosts.size)
        assertEquals(first.activeId, second.activeId)
        assertEquals("ZZZZ1234ZZZZ1234ZZZZ1234", second.active!!.pairing.token)
    }

    @Test
    fun `same host on a different port remains a distinct endpoint`() {
        val first = PairingRegistryCodec.upsert(PairingRegistry(), Pairing("tower", 9417, token))
        val second = PairingRegistryCodec.upsert(first, Pairing("tower", 19417, token))
        assertEquals(2, second.hosts.size)
    }

    @Test
    fun `corrupt and future snapshots fail closed`() {
        assertEquals(RegistryDecode.Corrupt, PairingRegistryCodec.decode("not json"))
        assertEquals(RegistryDecode.Future, PairingRegistryCodec.decode("""{"version":2,"hosts":[]}"""))
    }

    @Test
    fun `strict endpoint and token boundary rejects authority injection and local addresses`() {
        listOf("ws://tower", "tower:9417", "tower/path", "user@tower", "tower?x", "tower#x", "tower%2f", "127.0.0.1", "0.0.0.0", "localhost", "[::1]", "[::]").forEach {
            assertEquals("must reject $it", null, canonicalHost(it))
        }
        assertEquals("100.111.143.67", canonicalHost("100.111.143.67"))
        assertTrue(canonicalHost("[fd7a:115c:a1e0::1]") != null)
        assertEquals(token, normalizePairingToken("abcd-1234 abcd-1234 abcd-1234"))
        listOf("I".repeat(24), "A".repeat(23), "A".repeat(25), "A".repeat(23) + "!").forEach {
            assertEquals(null, normalizePairingToken(it))
        }
        assertEquals(null, normalizePairingToken("\u017f" + "A".repeat(23)))
    }
}
