package com.greponlabs.navette.ui.connect

import com.greponlabs.navette.net.Pairing
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * Manual entry is the only pairing path on a device without Play Services, so
 * it has to accept everything the QR path does -- including a non-default port.
 */
class ManualPairingTest {
    private val token = "ABCD1234ABCD1234ABCD1234"
    @Test
    fun `builds a pairing on a non default port`() {
        assertEquals(
            Pairing("tower.ts.net", 19417, "ABCD1234ABCD1234ABCD1234"),
            manualPairing("tower.ts.net", "19417", token),
        )
    }

    @Test
    fun `trims surrounding whitespace on every field`() {
        // A pasted host or token routinely arrives with a trailing space, and a
        // port field is no different.
        assertEquals(
            Pairing("tower", 9417, "ABCD1234ABCD1234ABCD1234"),
            manualPairing("  tower ", " 9417 ", " $token "),
        )
    }

    @Test
    fun `rejects a port outside the legal range`() {
        // The same 1..65535 bound parsePairingUri applies, so a typed pairing
        // and a scanned one accept exactly the same values.
        assertNull(manualPairing("tower", "0", token))
        assertNull(manualPairing("tower", "65536", token))
        assertNull(manualPairing("tower", "-1", token))
    }

    @Test
    fun `rejects a port that is not a number`() {
        assertNull(manualPairing("tower", "", token))
        assertNull(manualPairing("tower", "9417a", token))
        assertNull(manualPairing("tower", "94 17", token))
        // Well past Int range -- toIntOrNull returns null rather than wrapping.
        assertNull(manualPairing("tower", "99999999999", token))
    }

    @Test
    fun `the field's error state and the pair button agree on every port`() {
        // Both read `parsePort`; this pins that they cannot diverge. A field
        // that rejected what the button accepts (or vice versa) is a dead-end
        // form with no visible reason.
        for (port in listOf("9417", "1", "65535", "0", "65536", "", "x", " 9417 ")) {
            assertEquals(
                "port $port: field validity and pairing validity must agree",
                parsePort(port) != null,
                manualPairing("tower", port, token) != null,
            )
        }
    }

    @Test
    fun `rejects a blank host or token`() {
        assertNull(manualPairing("", "9417", token))
        assertNull(manualPairing("   ", "9417", token))
        assertNull(manualPairing("tower", "9417", ""))
        assertNull(manualPairing("tower", "9417", "   "))
    }
}
