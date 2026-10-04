package com.greponlabs.navette.net

import android.annotation.SuppressLint
import android.content.Context
import android.net.wifi.WifiManager
import android.os.Build
import android.util.Log

/** The lock [StreamWifiLock] drives; an interface because `WifiManager.WifiLock` is an android.jar stub in JVM tests. */
internal interface WifiLockHandle {
    val isHeld: Boolean

    fun acquire()

    fun release()
}

/**
 * Keeps the phone's Wi-Fi radio out of power save while a session is on screen.
 *
 * In power save the access point buffers downlink packets until the radio next
 * wakes: at a beacon, or when the phone itself transmits. A desktop stream
 * sends a frame only when something changes, so the frame for one keystroke
 * could wait at the access point until the next keystroke woke the radio.
 * Measured on the Pixel 10 (2026-10-03): with the radio idle, the first segment
 * of a frame took 51-114 ms to be acknowledged; kept awake, 10-22 ms.
 * Host-to-phone pings ran 7-288 ms against 13-29 ms phone-to-host.
 *
 * Only [WifiManager.WIFI_MODE_FULL_LOW_LATENCY] (API 29+) is used: the system
 * honours it only while the app is in the foreground with the screen on, so the
 * battery cost ends when the user leaves. Below 29 the only mode that helps
 * is `WIFI_MODE_FULL_HIGH_PERF`, which also holds in the background, so those
 * devices go without.
 */
internal class StreamWifiLock(private val handle: WifiLockHandle) {
    fun acquire() {
        if (handle.isHeld) return
        runCatching { handle.acquire() }
            .onFailure { Log.w(TAG, "could not keep Wi-Fi out of power save: ${it.message}") }
    }

    fun release() {
        if (!handle.isHeld) return
        runCatching { handle.release() }
            .onFailure { Log.w(TAG, "could not release the Wi-Fi lock: ${it.message}") }
    }

    companion object {
        private const val TAG = "StreamWifiLock"

        /** Null when the platform has no suitable mode or the device has no Wi-Fi service. */
        fun create(context: Context): StreamWifiLock? {
            val mode = streamWifiLockMode(Build.VERSION.SDK_INT) ?: return null
            // Application context: lint's WifiManagerLeak expects it, and the lock
            // belongs to no particular Activity.
            val wifi = context.applicationContext.getSystemService(WifiManager::class.java) ?: return null
            val lock = wifi.createWifiLock(mode, "navette:stream").apply { setReferenceCounted(false) }
            return StreamWifiLock(
                object : WifiLockHandle {
                    override val isHeld: Boolean get() = lock.isHeld

                    override fun acquire() = lock.acquire()

                    override fun release() = lock.release()
                },
            )
        }
    }
}

// The constant is inlined at compile time; lint cannot see that `sdkInt` gates it.
@SuppressLint("InlinedApi")
internal fun streamWifiLockMode(sdkInt: Int): Int? =
    if (sdkInt >= Build.VERSION_CODES.Q) WifiManager.WIFI_MODE_FULL_LOW_LATENCY else null
