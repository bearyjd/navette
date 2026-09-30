package com.greponlabs.navette.net

import org.junit.Assert.assertEquals
import org.junit.Test

class ConnectionPhaseTest {
    @Test
    fun `every connection state has exactly the phase the UI acts on`() {
        assertEquals(ConnectionPhase.Connecting, ConnectionState.Connecting.phase)
        assertEquals(ConnectionPhase.Live, ConnectionState.Connected.phase)
        assertEquals(ConnectionPhase.Dropped, ConnectionState.Disconnected.phase)
        assertEquals(ConnectionPhase.Dropped, ConnectionState.Failed("reset").phase)
        // Terminal, and never retried: its own phase so no caller can lump it in with a drop.
        assertEquals(ConnectionPhase.Rejected, ConnectionState.Unauthorized.phase)
    }
}
