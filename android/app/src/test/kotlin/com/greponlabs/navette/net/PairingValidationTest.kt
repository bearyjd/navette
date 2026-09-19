package com.greponlabs.navette.net

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * `canonicalMac` is the phone's half of the MAC grammar `navetted`'s
 * `/v1/wake` accepts. It has to agree with the daemon in both directions: a
 * MAC the phone refuses is one the user cannot enter at all, and one it accepts
 * but the daemon does not is a 400 at the worst moment.
 */
class PairingValidationTest {
    private val canonical = "aa:bb:cc:dd:ee:ff"

    @Test
    fun `accepts colon, dash and bare forms in any case, and canonicalizes to lowercase colons`() {
        listOf(
            "aa:bb:cc:dd:ee:ff",
            "AA:BB:CC:DD:EE:FF",
            "Aa:bB:cC:Dd:eE:fF",
            "aa-bb-cc-dd-ee-ff",
            "AA-BB-CC-DD-EE-FF",
            "aabbccddeeff",
            "AABBCCDDEEFF",
        ).forEach { assertEquals("must canonicalize $it", canonical, canonicalMac(it)) }
        assertEquals("00:11:22:33:44:55", canonicalMac("001122334455"))
    }

    @Test
    fun `grammar only, so broadcast and all-zero addresses pass`() {
        // The daemon decides what it will send; a second, stricter definition
        // here would refuse what it accepts.
        assertEquals("ff:ff:ff:ff:ff:ff", canonicalMac("FF:FF:FF:FF:FF:FF"))
        assertEquals("00:00:00:00:00:00", canonicalMac("00:00:00:00:00:00"))
    }

    @Test
    fun `rejects the wrong length, mixed separators, non-hex digits and whitespace`() {
        listOf(
            "",
            "aa:bb:cc:dd:ee",
            "aa:bb:cc:dd:ee:ff:00",
            "aabbccddeef",
            "aabbccddeeff0",
            "aa:bb-cc:dd-ee:ff",
            "aa.bb.cc.dd.ee.ff",
            "aabb.ccdd.eeff",
            "aa:bb:cc:dd:ee:gg",
            "aa:bb:cc:dd:ee:f",
            "aa::bb:cc:dd:ee:ff",
            ":aa:bb:cc:dd:ee:ff",
            "aa:bb:cc:dd:ee:ff:",
            " aa:bb:cc:dd:ee:ff",
            "aa:bb:cc:dd:ee:ff ",
            "aa:bb:cc:dd:ee:ff\n",
            "aa bb cc dd ee ff",
            "zz:zz:zz:zz:zz:zz",
        ).forEach { assertNull("must reject '$it'", canonicalMac(it)) }
    }

    @Test
    fun `rejects non-ascii look-alikes rather than case folding them`() {
        // Fullwidth 'Ａ' and a Cyrillic 'а' are not hex digits, whatever
        // a lenient uppercase might make of them.
        assertNull(canonicalMac("Ａa:bb:cc:dd:ee:ff"))
        assertNull(canonicalMac("аa:bb:cc:dd:ee:ff"))
    }
}
