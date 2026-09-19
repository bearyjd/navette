package com.greponlabs.navette.ui.connect

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
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
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import com.google.mlkit.vision.codescanner.GmsBarcodeScanning
import com.greponlabs.navette.net.ConnectionState
import com.greponlabs.navette.net.DEFAULT_NAVETTE_PORT
import com.greponlabs.navette.net.Pairing
import com.greponlabs.navette.net.canonicalHost
import com.greponlabs.navette.net.canonicalPort
import com.greponlabs.navette.net.normalizePairingToken
import com.greponlabs.navette.net.parsePairingUri
import com.greponlabs.navette.net.validatedPairing
import com.greponlabs.navette.ui.WakeUiState

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
 * A successful pairing upserts the encrypted host registry; re-pairing the
 * same endpoint replaces only that endpoint's rotated token.
 */
/**
 * Builds a [Pairing] from the manual-entry fields, or null when they are not
 * yet a usable pairing.
 *
 * Extracted from the composable so that the "Pair" button's enabled state and
 * the keyboard's Go action cannot drift apart -- they were already two
 * structurally different predicates, and adding a fallible port to only one of
 * them would give Go a path to pair on a port the button refuses.
 *
 * The 1..65535 bound matches `parsePairingUri`, so a typed pairing and a
 * scanned one accept exactly the same values.
 */
internal fun manualPairing(host: String, port: String, token: String): Pairing? {
    val parsedPort = parsePort(port) ?: return null
    return validatedPairing(host.trim(), parsedPort, token.trim())
}

/**
 * The single definition of an acceptable port, so the field's error state and
 * the Pair button cannot disagree about one. `PairingUri.kt` declines to
 * duplicate the token's shape for the same reason: a second definition is a
 * second thing to drift.
 */
internal fun parsePort(raw: String): Int? = canonicalPort(raw.trim())

/**
 * [wakeViaLabel] is the relay a failed host can be woken through, or null when
 * it has no wake target; [wake] is where the last wake attempt got to, and
 * [onWake] sends one.
 */
@Composable
fun ConnectScreen(
    connection: ConnectionState,
    onPaired: (Pairing) -> Unit,
    onRetry: () -> Unit,
    onBack: (() -> Unit)? = null,
    onManageHosts: (() -> Unit)? = null,
    wake: WakeUiState = WakeUiState.Idle,
    wakeViaLabel: String? = null,
    onWake: () -> Unit = {},
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    val onPairedState = rememberUpdatedState(onPaired)
    var scanError by rememberSaveable { mutableStateOf<String?>(null) }
    var manualEntryShown by rememberSaveable { mutableStateOf(false) }
    var manualHost by rememberSaveable { mutableStateOf("") }
    // Manual entry is the only pairing path on a device without Play Services,
    // so it has to reach the same daemons the QR path can: that URI carries a
    // real port, and hardcoding the default here left those devices unable to
    // reach a daemon bound anywhere else. Prefilled, so the common case is
    // still no typing.
    var manualPort by rememberSaveable { mutableStateOf(DEFAULT_NAVETTE_PORT.toString()) }
    // `remember`, deliberately not `rememberSaveable`: saved instance state is a
    // Bundle the OS holds outside our encrypted store, survives process death,
    // and can reach disk. A typed token does not belong there. The cost is that
    // a rotation mid-entry clears the field, which is the right trade for a
    // secret in a one-time pairing flow.
    var manualToken by remember { mutableStateOf("") }
    // Masked by default: this field exists to keep the token out of logs,
    // Debug output and error strings, and a plaintext field on screen would
    // undo that in a different medium (over-the-shoulder in public, screen
    // recording). The toggle exists because a 24-char base32 string typed
    // fully blind is genuinely error-prone, and a mistyped token fails at
    // connect time looking exactly like an auth bug.
    var tokenVisible by rememberSaveable { mutableStateOf(false) }

    val connecting = connection is ConnectionState.Connecting

    Column(
        modifier = modifier.fillMaxSize().padding(32.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp, Alignment.CenterVertically),
    ) {
        Text(text = "Navette", style = MaterialTheme.typography.headlineMedium)
        if (onBack != null) TextButton(onClick = onBack) { Text("Back to computers") }
        if (onManageHosts != null) TextButton(onClick = onManageHosts) { Text("Saved computers") }
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
            FailedActions(wake = wake, wakeViaLabel = wakeViaLabel, onRetry = onRetry, onWake = onWake)
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
            // One predicate for both the Go action and the Pair button, so
            // neither can accept input the other rejects.
            val typedPairing = manualPairing(manualHost, manualPort, manualToken)
            OutlinedTextField(
                value = manualHost,
                onValueChange = { manualHost = it },
                label = { Text("Host (e.g. tower or 100.x.x.x)") },
                singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri, imeAction = ImeAction.Next),
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = manualPort,
                onValueChange = { manualPort = it },
                label = { Text("Port") },
                singleLine = true,
                isError = manualPort.isNotBlank() && parsePort(manualPort) == null,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number, imeAction = ImeAction.Next),
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = manualToken,
                onValueChange = { manualToken = it },
                label = { Text("Token") },
                singleLine = true,
                visualTransformation = if (tokenVisible) VisualTransformation.None else PasswordVisualTransformation(),
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password, imeAction = ImeAction.Go),
                keyboardActions =
                    KeyboardActions(
                        onGo = { typedPairing?.let(onPaired) },
                    ),
                // A TextButton, not an IconButton: this project has no Material
                // icons dependency, and its own text content ("Show"/"Hide") is
                // what a TalkBack user hears -- an icon-only button would need a
                // separate contentDescription to reach the same accessibility bar.
                trailingIcon = {
                    TextButton(onClick = { tokenVisible = !tokenVisible }) {
                        Text(if (tokenVisible) "Hide" else "Show")
                    }
                },
                modifier = Modifier.fillMaxWidth(),
            )
            Button(
                onClick = { typedPairing?.let(onPaired) },
                enabled = typedPairing != null && !connecting,
            ) {
                Text("Pair")
            }
        }
    }
}

/**
 * Retry, and -- for a host with a wake target -- the Wake button beside it.
 * The button stays after a failed wake so a relay hiccup costs one tap, not a
 * Retry round trip; it goes away after a successful one, because the useful
 * next step is Retry, and a second packet does nothing the first did not.
 */
@Composable
private fun FailedActions(
    wake: WakeUiState,
    wakeViaLabel: String?,
    onRetry: () -> Unit,
    onWake: () -> Unit,
) {
    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        Button(onClick = onRetry) {
            Text("Retry")
        }
        if (wakeViaLabel != null && wake !is WakeUiState.Sent) {
            Button(onClick = onWake, enabled = wake !is WakeUiState.Sending) {
                Text("Wake via $wakeViaLabel")
            }
        }
    }
    when (wake) {
        WakeUiState.Idle, WakeUiState.Sending -> Unit
        is WakeUiState.Sent -> Text("Magic packet sent via ${wake.viaLabel} — give the host a minute, then Retry.")
        is WakeUiState.Failed -> Text(text = wake.message, color = MaterialTheme.colorScheme.error)
    }
}
