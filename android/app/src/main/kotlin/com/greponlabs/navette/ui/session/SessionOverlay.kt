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
import com.greponlabs.navette.net.ConnectionPhase
import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.phase

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
    if (terminal == null && state.connection.phase == ConnectionPhase.Live && state.contentSize != null) return

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
                // Exhaustive over the phase, no `else`: a new ConnectionState
                // must be given a phase before this compiles, and then lands in
                // a branch someone chose (see ConnectionPhase).
                else ->
                    when (state.connection.phase) {
                        // Ahead of `reconnecting`: the daemon refused our token,
                        // so retrying is not merely unhelpful here, it is the
                        // exact silent-loop failure mode this state exists to
                        // prevent (see ReconnectPolicy.shouldRetry). The only way
                        // out is pairing again, not a reconnect button that would
                        // just be refused the same way.
                        ConnectionPhase.Rejected -> {
                            OverlayText("Pairing rejected -- scan the QR code again", MaterialTheme.typography.bodyLarge)
                            TextButton(onClick = onLeave) { Text("Back to sessions", color = Color.White) }
                        }
                        ConnectionPhase.Dropped ->
                            if (reconnecting) {
                                ReconnectingOverlay(onLeave)
                            } else {
                                val reason = (state.connection as? ConnectionState.Failed)?.reason ?: "connection lost"
                                OverlayText("Disconnected: $reason", MaterialTheme.typography.bodyLarge)
                                Button(onClick = onReconnect) { Text("Reconnect") }
                                TextButton(onClick = onLeave) { Text("Back to sessions", color = Color.White) }
                            }
                        ConnectionPhase.Connecting ->
                            if (reconnecting) {
                                ReconnectingOverlay(onLeave)
                            } else {
                                val label =
                                    if (reconnectAttempt > 0) "Reconnecting... ($reconnectAttempt/$maxAttempts)" else "Connecting..."
                                SpinnerOverlay(label)
                            }
                        ConnectionPhase.Live ->
                            if (reconnecting) ReconnectingOverlay(onLeave) else SpinnerOverlay("Waiting for the first frame...")
                    }
            }
        }
    }
}

@Composable
private fun ReconnectingOverlay(onLeave: () -> Unit) {
    CircularProgressIndicator()
    OverlayText("Connection lost -- reconnecting...", MaterialTheme.typography.bodyMedium)
    TextButton(onClick = onLeave) { Text("Back to sessions", color = Color.White) }
}

@Composable
private fun SpinnerOverlay(label: String) {
    CircularProgressIndicator()
    OverlayText(label, MaterialTheme.typography.bodyMedium)
}

@Composable
private fun OverlayText(text: String, style: TextStyle) {
    Text(text = text, color = Color.White, style = style, modifier = Modifier.padding(horizontal = 24.dp))
}
