package com.greponlabs.navette.ui.connect

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.greponlabs.navette.net.ConnectionState

/**
 * A tailnet hostname or IP, not a saved multi-host registry -- that's M4
 * (docs/ROADMAP.md, "Cloud + polish"). One box, one Connect button.
 */
@Composable
fun ConnectScreen(
    host: String,
    connection: ConnectionState,
    onHostChanged: (String) -> Unit,
    onConnect: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Column(
        modifier = modifier.fillMaxSize().padding(32.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp, Alignment.CenterVertically),
    ) {
        Text(text = "Navette", style = MaterialTheme.typography.headlineMedium)
        Text(text = "Enter a host on your tailnet running navetted.")
        OutlinedTextField(
            value = host,
            onValueChange = onHostChanged,
            label = { Text("Host (e.g. tower or 100.x.x.x)") },
            singleLine = true,
            keyboardOptions =
                KeyboardOptions(
                    keyboardType = KeyboardType.Uri,
                    imeAction = ImeAction.Go,
                ),
            keyboardActions = KeyboardActions(onGo = { onConnect() }),
            modifier = Modifier.fillMaxWidth(),
        )
        Button(onClick = onConnect, enabled = host.isNotBlank() && connection !is ConnectionState.Connecting) {
            Text(if (connection is ConnectionState.Connecting) "Connecting..." else "Connect")
        }
        if (connection is ConnectionState.Connecting) {
            CircularProgressIndicator()
        }
        if (connection is ConnectionState.Failed) {
            Text(
                text = connection.reason,
                color = MaterialTheme.colorScheme.error,
            )
        }
    }
}
