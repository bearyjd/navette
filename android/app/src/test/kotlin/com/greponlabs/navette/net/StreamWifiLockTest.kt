package com.greponlabs.navette.net

import android.net.wifi.WifiManager
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class StreamWifiLockTest {
    private class FakeLock(
        private val acquireFailure: RuntimeException? = null,
        private val releaseFailure: RuntimeException? = null,
    ) : WifiLockHandle {
        var acquires = 0
        var releases = 0
        override var isHeld = false
            private set

        override fun acquire() {
            acquires++
            acquireFailure?.let { throw it }
            isHeld = true
        }

        override fun release() {
            releases++
            releaseFailure?.let { throw it }
            isHeld = false
        }
    }

    @Test
    fun `low latency mode from Android 10 on`() {
        assertEquals(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, streamWifiLockMode(29))
        assertEquals(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, streamWifiLockMode(36))
    }

    @Test
    fun `no lock before Android 10, where the only mode outlives the foreground`() {
        assertNull(streamWifiLockMode(28))
        assertNull(streamWifiLockMode(26))
    }

    @Test
    fun `acquire holds the lock and release lets it go`() {
        val fake = FakeLock()
        val lock = StreamWifiLock(fake)

        lock.acquire()
        assertTrue(fake.isHeld)
        assertEquals(1, fake.acquires)

        lock.release()
        assertFalse(fake.isHeld)
        assertEquals(1, fake.releases)
    }

    @Test
    fun `release without a held lock touches nothing`() {
        val fake = FakeLock()
        StreamWifiLock(fake).release()
        assertEquals(0, fake.releases)
    }

    @Test
    fun `a refused acquire is swallowed and leaves nothing to release`() {
        val fake = FakeLock(acquireFailure = SecurityException("WAKE_LOCK missing"))
        val lock = StreamWifiLock(fake)

        lock.acquire()
        lock.release()

        assertEquals(1, fake.acquires)
        assertFalse(fake.isHeld)
        assertEquals(0, fake.releases)
    }

    @Test
    fun `a second acquire while held is not stacked`() {
        val fake = FakeLock()
        val lock = StreamWifiLock(fake)

        lock.acquire()
        lock.acquire()

        assertEquals(1, fake.acquires)
    }

    @Test
    fun `a failing release is swallowed, since it runs on every way out of a session`() {
        val fake = FakeLock(releaseFailure = IllegalStateException("system server gone"))
        val lock = StreamWifiLock(fake)

        lock.acquire()
        lock.release()

        assertEquals(1, fake.releases)
    }
}
