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
        assertEquals(RegistryDecode.Corrupt, PairingRegistryCodec.decode("""{"version":0,"hosts":[]}"""))
        assertEquals(RegistryDecode.Future, PairingRegistryCodec.decode("""{"version":3,"hosts":[]}"""))
    }

    // -- schema v2: wake targets --------------------------------------------

    private fun hostJson(id: String, host: String, extra: String = "") =
        """{"id":"$id","host":"$host","port":9417,"token":"$token"$extra}"""

    private fun v2(hosts: String, activeId: String = "a") = """{"version":2,"activeId":"$activeId","hosts":[$hosts]}"""

    private val twoHosts = PairingRegistryCodec.upsert(PairingRegistryCodec.upsert(PairingRegistry(), Pairing("tower", 9417, token)), Pairing("nas", 9417, token))
    private val towerId get() = twoHosts.hosts.first { it.pairing.host == "tower" }.id
    private val nasId get() = twoHosts.hosts.first { it.pairing.host == "nas" }.id

    private fun decodeValid(raw: String): PairingRegistry = (PairingRegistryCodec.decode(raw) as RegistryDecode.Valid).registry

    @Test
    fun `a version 1 snapshot still decodes, with no wake target`() {
        val v1 = """{"version":1,"activeId":"a","hosts":[${hostJson("a", "tower")}]}"""
        val registry = decodeValid(v1)
        assertEquals("tower", registry.active!!.pairing.host)
        assertEquals(null, registry.active!!.wake)
    }

    @Test
    fun `a version 1 snapshot carrying wake fields is corrupt, not an early adopter`() {
        // No shipped v1 writer ever produced these keys.
        val v1 = """{"version":1,"hosts":[${hostJson("a", "tower", ""","mac":"aa:bb:cc:dd:ee:ff","wakeViaId":"b"""")},${hostJson("b", "nas")}]}"""
        assertEquals(RegistryDecode.Corrupt, PairingRegistryCodec.decode(v1))
    }

    @Test
    fun `encode writes version 2 and a wake target round trips`() {
        val registry = PairingRegistryCodec.setWake(twoHosts, towerId, WakeTarget("AA-BB-CC-DD-EE-FF", nasId))
        val encoded = PairingRegistryCodec.encode(registry)
        assertTrue(encoded, encoded.contains(""""version":2"""))
        val decoded = decodeValid(encoded)
        assertEquals(WakeTarget("aa:bb:cc:dd:ee:ff", nasId), decoded.hosts.first { it.id == towerId }.wake)
        assertEquals(null, decoded.hosts.first { it.id == nasId }.wake)
        assertEquals(registry, decoded)
    }

    @Test
    fun `a host without a wake target encodes without wake keys`() {
        // Compact, and the shape a v2 reader already accepts as "no wake".
        val encoded = PairingRegistryCodec.encode(twoHosts)
        assertFalse(encoded, encoded.contains("mac"))
        assertFalse(encoded, encoded.contains("wakeViaId"))
    }

    /**
     * Wake metadata is recoverable; the pairings it hangs off are not. A bad
     * wake target therefore degrades to "no wake target" -- `loadRegistry`
     * maps Corrupt to an empty registry and the next write overwrites the
     * blob, so a Corrupt here would trade every host and token for a MAC.
     */
    private fun assertDecodesWithoutWake(raw: String) {
        val registry = decodeValid(raw)
        assertEquals(listOf("a", "b"), registry.hosts.map { it.id })
        assertTrue("every wake target must be dropped, got ${registry.hosts}", registry.hosts.all { it.wake == null })
        assertEquals("tower", registry.hosts.first { it.id == "a" }.pairing.host)
    }

    @Test
    fun `a mac without a relay, or a relay without a mac, decodes with no wake target`() {
        assertDecodesWithoutWake(v2("${hostJson("a", "tower", ""","mac":"aa:bb:cc:dd:ee:ff"""")},${hostJson("b", "nas")}"))
        assertDecodesWithoutWake(v2("${hostJson("a", "tower", ""","wakeViaId":"b"""")},${hostJson("b", "nas")}"))
    }

    @Test
    fun `a relay that is not in the registry is dropped, keeping the host`() {
        assertDecodesWithoutWake(v2("${hostJson("a", "tower", ""","mac":"aa:bb:cc:dd:ee:ff","wakeViaId":"ghost"""")},${hostJson("b", "nas")}"))
    }

    @Test
    fun `a host that relays through itself loses its wake target, not its pairing`() {
        assertDecodesWithoutWake(v2("${hostJson("a", "tower", ""","mac":"aa:bb:cc:dd:ee:ff","wakeViaId":"a"""")},${hostJson("b", "nas")}"))
    }

    @Test
    fun `an invalid mac drops the wake target, keeping the host`() {
        assertDecodesWithoutWake(v2("${hostJson("a", "tower", ""","mac":"aa:bb:cc:dd:ee","wakeViaId":"b"""")},${hostJson("b", "nas")}"))
    }

    @Test
    fun `one bad wake target does not take down the good one`() {
        val mixed = v2(
            "${hostJson("a", "tower", ""","mac":"aa:bb:cc:dd:ee:ff","wakeViaId":"ghost"""")}," +
                "${hostJson("b", "nas", ""","mac":"11:22:33:44:55:66","wakeViaId":"a"""")}",
        )
        val registry = decodeValid(mixed)
        assertEquals(listOf("a", "b"), registry.hosts.map { it.id })
        assertEquals(null, registry.hosts.first { it.id == "a" }.wake)
        assertEquals(WakeTarget("11:22:33:44:55:66", "a"), registry.hosts.first { it.id == "b" }.wake)
        // And what came out is something encode will take back in unchanged.
        assertEquals(registry, decodeValid(PairingRegistryCodec.encode(registry)))
    }

    @Test
    fun `pairing-level damage is still corrupt, whatever the wake fields say`() {
        // The degrade rule is for wake metadata only: a host, token or id that
        // cannot be trusted is not something to carry on with.
        val badHost = v2("""{"id":"a","host":"127.0.0.1","port":9417,"token":"$token","mac":"aa:bb:cc:dd:ee:ff","wakeViaId":"b"},${hostJson("b", "nas")}""")
        val badToken = v2("""{"id":"a","host":"tower","port":9417,"token":"nope"},${hostJson("b", "nas")}""")
        val dupIds = v2("${hostJson("a", "tower")},${hostJson("a", "nas")}")
        val danglingActive = v2("${hostJson("a", "tower")},${hostJson("b", "nas")}", activeId = "ghost")
        listOf(badHost, badToken, dupIds, danglingActive).forEach {
            assertEquals("must be corrupt: $it", RegistryDecode.Corrupt, PairingRegistryCodec.decode(it))
        }
    }

    @Test
    fun `setWake stores a canonical mac on the named host only, and clears with null`() {
        val set = PairingRegistryCodec.setWake(twoHosts, towerId, WakeTarget("AABBCCDDEEFF", nasId))
        assertEquals(WakeTarget("aa:bb:cc:dd:ee:ff", nasId), set.hosts.first { it.id == towerId }.wake)
        assertEquals(null, set.hosts.first { it.id == nasId }.wake)
        assertEquals("the original registry is not mutated", null, twoHosts.hosts.first { it.id == towerId }.wake)
        assertEquals(twoHosts.activeId, set.activeId)

        val cleared = PairingRegistryCodec.setWake(set, towerId, null)
        assertEquals(twoHosts, cleared)
    }

    @Test
    fun `setWake rejects an unknown host, an unknown relay, a self relay and a bad mac`() {
        val good = WakeTarget("aa:bb:cc:dd:ee:ff", nasId)
        assertThrows<IllegalArgumentException>("unknown host") { PairingRegistryCodec.setWake(twoHosts, "ghost", good) }
        assertThrows<IllegalArgumentException>("unknown relay") { PairingRegistryCodec.setWake(twoHosts, towerId, good.copy(viaId = "ghost")) }
        assertThrows<IllegalArgumentException>("self relay") { PairingRegistryCodec.setWake(twoHosts, towerId, good.copy(viaId = towerId)) }
        assertThrows<IllegalArgumentException>("bad mac") { PairingRegistryCodec.setWake(twoHosts, towerId, good.copy(mac = "aa:bb:cc:dd:ee:gg")) }
        // Clearing an unknown host is still an error: there is nothing to clear.
        assertThrows<IllegalArgumentException>("unknown host, null") { PairingRegistryCodec.setWake(twoHosts, "ghost", null) }
    }

    @Test
    fun `re-pairing a host keeps its wake target`() {
        // The wake target belongs to the machine; re-pairing only rotates the token.
        val withWake = PairingRegistryCodec.setWake(twoHosts, towerId, WakeTarget("aa:bb:cc:dd:ee:ff", nasId))
        val repaired = PairingRegistryCodec.upsert(withWake, Pairing("TOWER", 9417, "ZZZZ1234ZZZZ1234ZZZZ1234"))
        val tower = repaired.hosts.first { it.id == towerId }
        assertEquals("ZZZZ1234ZZZZ1234ZZZZ1234", tower.pairing.token)
        assertEquals(WakeTarget("aa:bb:cc:dd:ee:ff", nasId), tower.wake)
        assertEquals(2, repaired.hosts.size)
    }

    @Test
    fun `removing a relay clears the wake targets that pointed at it`() {
        val withWake = PairingRegistryCodec.setWake(twoHosts, towerId, WakeTarget("aa:bb:cc:dd:ee:ff", nasId))
        val removed = PairingRegistryCodec.remove(withWake, nasId)
        assertEquals(listOf(towerId), removed.hosts.map { it.id })
        assertEquals(null, removed.hosts.single().wake)
        assertEquals("the active host was removed, so nothing is active", null, removed.activeId)
        // What this protects: the result must still be something encode accepts and decode reads back.
        assertEquals(removed, decodeValid(PairingRegistryCodec.encode(removed)))
    }

    @Test
    fun `removing a host that relays through another leaves the relay untouched`() {
        val withWake = PairingRegistryCodec.setWake(twoHosts, towerId, WakeTarget("aa:bb:cc:dd:ee:ff", nasId))
        val removed = PairingRegistryCodec.remove(withWake, towerId)
        assertEquals(listOf(nasId), removed.hosts.map { it.id })
        assertEquals(nasId, removed.activeId)
    }

    @Test
    fun `encode refuses a dangling wake target rather than persisting it`() {
        // Belt and braces for whatever bypasses remove(): a registry decode would refuse must never be written.
        val dangling = PairingRegistry(listOf(SavedPairing(towerId, Pairing("tower", 9417, token), WakeTarget("aa:bb:cc:dd:ee:ff", "ghost"))), towerId)
        assertThrows<IllegalStateException>("dangling") { PairingRegistryCodec.encode(dangling) }
    }

    private inline fun <reified T : Throwable> assertThrows(label: String, block: () -> Unit) {
        val thrown = runCatching(block).exceptionOrNull()
        assertTrue("$label: expected ${T::class.simpleName}, got $thrown", thrown is T)
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
