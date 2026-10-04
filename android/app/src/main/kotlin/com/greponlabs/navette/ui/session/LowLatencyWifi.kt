package com.greponlabs.navette.ui.session

import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.ui.platform.LocalContext
import com.greponlabs.navette.net.StreamWifiLock

/**
 * Holds Wi-Fi out of power save for as long as the session screen is composed
 * (see [StreamWifiLock] for why). Leaving the app needs no handling here: the
 * system suspends a low-latency lock whenever the app is not in the foreground
 * with the screen on, and resumes it on return.
 */
@Composable
internal fun HoldLowLatencyWifiWhileAttached() {
    val context = LocalContext.current
    DisposableEffect(context) {
        val lock = StreamWifiLock.create(context)
        lock?.acquire()
        onDispose { lock?.release() }
    }
}
