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
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.google.mlkit.vision.codescanner.GmsBarcodeScanning
import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.DEFAULT_NAVETTE_PORT
import com.greponlabs.navette.net.Pairing
import com.greponlabs.navette.net.parsePairingUri

/**
 * Pairs with a `navetted` host by scanning the QR code it renders
 * (`navette token --qr`), or by typing a host and token by hand.
 *
 * The scanner needs no camera permission: `GmsBarcodeScanning` runs the
 * camera and QR decode inside Play Services' own full-screen UI, out of this
 * app's process entirely, which is why there is no permission request here
 * and no CameraX preview to wire up. Manual entry stays because Play
 * Services is not guaranteed to be present, and because it is the only way
 * to test pairing without a physical camera.
 *
 * Not a saved multi-host registry -- that's M4 (docs/ROADMAP.md, "Cloud +
 * polish"). One pairing, one slot in [com.greponlabs.navette.net.PairingStore].
 */
@Composable
fun ConnectScreen(
    connection: ConnectionState,
    onPaired: (Pairing) -> Unit,
    onRetry: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    val onPairedState = rememberUpdatedState(onPaired)
    var scanError by rememberSaveable { mutableStateOf<String?>(null) }
    var manualEntryShown by rememberSaveable { mutableStateOf(false) }
    var manualHost by rememberSaveable { mutableStateOf("") }
    var manualToken by rememberSaveable { mutableStateOf("") }

    val connecting = connection is ConnectionState.Connecting

    Column(
        modifier = modifier.fillMaxSize().padding(32.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp, Alignment.CenterVertically),
    ) {
        Text(text = "Navette", style = MaterialTheme.typography.headlineMedium)
        Text(text = "Scan the pairing QR code shown by \"navette token --qr\".")

        Button(
            onClick = {
                scanError = null
                GmsBarcodeScanning.getClient(context)
                    .startScan()
                    .addOnSuccessListener { barcode ->
                        val pairing = barcode.rawValue?.let(::parsePairingUri)
                        if (pairing == null) {
                            scanError = "That QR code is not a navette pairing code"
                        } else {
                            onPairedState.value(pairing)
                        }
                    }
                    .addOnFailureListener { error -> scanError = error.message ?: "Scan failed" }
            },
            enabled = !connecting,
        ) {
            Text("Scan pairing code")
        }

        if (connecting) {
            CircularProgressIndicator()
        }
        if (scanError != null) {
            Text(text = scanError.orEmpty(), color = MaterialTheme.colorScheme.error)
        }
        if (connection is ConnectionState.Failed) {
            Text(text = connection.reason, color = MaterialTheme.colorScheme.error)
            Button(onClick = onRetry, enabled = !connecting) {
                Text("Retry")
            }
        }
        // Terminal: the daemon rejected the stored token, and retrying it
        // would just fail the same way. Only a fresh pairing -- scanned or
        // typed -- can recover from here.
        if (connection is ConnectionState.Unauthorized) {
            Text(
                text = "Pairing rejected. Scan a new code or enter one manually.",
                color = MaterialTheme.colorScheme.error,
            )
        }

        TextButton(onClick = { manualEntryShown = !manualEntryShown }) {
            Text(if (manualEntryShown) "Hide manual entry" else "Enter manually")
        }
        if (manualEntryShown) {
            OutlinedTextField(
                value = manualHost,
                onValueChange = { manualHost = it },
                label = { Text("Host (e.g. tower or 100.x.x.x)") },
                singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri, imeAction = ImeAction.Next),
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = manualToken,
                onValueChange = { manualToken = it },
                label = { Text("Token") },
                singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password, imeAction = ImeAction.Go),
                keyboardActions =
                    KeyboardActions(
                        onGo = {
                            if (manualHost.isNotBlank() && manualToken.isNotBlank()) {
                                onPaired(Pairing(manualHost.trim(), DEFAULT_NAVETTE_PORT, manualToken.trim()))
                            }
                        },
                    ),
                modifier = Modifier.fillMaxWidth(),
            )
            Button(
                onClick = { onPaired(Pairing(manualHost.trim(), DEFAULT_NAVETTE_PORT, manualToken.trim())) },
                enabled = manualHost.isNotBlank() && manualToken.isNotBlank() && !connecting,
            ) {
                Text("Pair")
            }
        }
    }
}
