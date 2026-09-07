package com.greponlabs.navette.ui.session

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.unit.dp
import com.greponlabs.navette.net.ConnectionState

/**
 * Connection and error states drawn over the video.
 *
 * Precedence matters: a terminal state ([SessionUiState.decodeError], or the
 * guest window closing) wins over a reconnect, because retrying a stream that
 * is genuinely gone would loop forever. A [reconnecting] drop shows progress
 * and spends the retry budget on its own; only once that budget is exhausted
 * does the screen fall back to a manual [onReconnect]. A rebuild in flight
 * has its own wording, so a retry never reads as a first attach.
 */
@Composable
internal fun SessionOverlay(
    state: SessionUiState,
    reconnecting: Boolean,
    reconnectAttempt: Int,
    maxAttempts: Int,
    onReconnect: () -> Unit,
    onLeave: () -> Unit,
) {
    val terminal =
        when {
            state.decodeError != null -> state.decodeError
            state.streamEnded -> "The session's window closed."
            else -> null
        }

    // Nothing to draw once the stream is live: connected, a frame decoded, and
    // no terminal error. Everything else needs an overlay of some kind.
    if (terminal == null && state.connection is ConnectionState.Connected && state.contentSize != null) return

    Box(modifier = Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
        Column(
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            when {
                terminal != null -> {
                    OverlayText(terminal, MaterialTheme.typography.bodyLarge)
                    // A decoder failure may be transient; a rebuild gets a
                    // fresh codec. A closed window cannot be rebuilt.
                    if (state.decodeError != null) Button(onClick = onReconnect) { Text("Reconnect") }
                    TextButton(onClick = onLeave) { Text("Back to sessions", color = Color.White) }
                }
                reconnecting -> {
                    CircularProgressIndicator()
                    OverlayText("Connection lost -- reconnecting...", MaterialTheme.typography.bodyMedium)
                    TextButton(onClick = onLeave) { Text("Back to sessions", color = Color.White) }
                }
                ReconnectPolicy.isDropped(state.connection) -> {
                    val reason = (state.connection as? ConnectionState.Failed)?.reason ?: "connection lost"
                    OverlayText("Disconnected: $reason", MaterialTheme.typography.bodyLarge)
                    Button(onClick = onReconnect) { Text("Reconnect") }
                    TextButton(onClick = onLeave) { Text("Back to sessions", color = Color.White) }
                }
                else -> {
                    CircularProgressIndicator()
                    val label =
                        when {
                            state.connection is ConnectionState.Connecting && reconnectAttempt > 0 ->
                                "Reconnecting... ($reconnectAttempt/$maxAttempts)"
                            state.connection is ConnectionState.Connecting -> "Connecting..."
                            else -> "Waiting for the first frame..."
                        }
                    OverlayText(label, MaterialTheme.typography.bodyMedium)
                }
            }
        }
    }
}

@Composable
private fun OverlayText(text: String, style: TextStyle) {
    Text(text = text, color = Color.White, style = style, modifier = Modifier.padding(horizontal = 24.dp))
}
