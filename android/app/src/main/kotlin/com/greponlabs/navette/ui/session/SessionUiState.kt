package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.ViewScale

/** What the screen renders. */
internal data class SessionUiState(
    // Connecting, not Disconnected: the controller opens the socket from a
    // DisposableEffect, which runs after the first composition, so a
    // Disconnected default would flash a "Disconnected" overlay on entry
    // every time.
    val connection: ConnectionState = ConnectionState.Connecting,
    val streamEnded: Boolean = false,
    val decodeError: String? = null,
    /** The size of the frame currently on the surface, or `null` before the first one. */
    val contentSize: Pair<Int, Int>? = null,
    /** The latest metrics sample, or `null` before the first one. */
    val hud: HudSample? = null,
    /** The logical scale the viewport is reported at; what the Scale menu marks. */
    val viewScale: ViewScale = ViewScale.X1,
)
