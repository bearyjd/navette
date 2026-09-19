package com.greponlabs.navette.ui.hosts

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.greponlabs.navette.net.SavedPairing
import com.greponlabs.navette.net.WakeTarget
import com.greponlabs.navette.net.canonicalMac

/**
 * The one definition of a saveable wake target, so the Save button and the
 * field's error state cannot disagree -- the same reason `manualPairing`
 * exists on ConnectScreen. Trims, as manual pairing does: a pasted MAC
 * routinely arrives with a trailing space.
 */
internal fun wakeTargetFrom(mac: String, viaId: String?): WakeTarget? {
    val canonical = canonicalMac(mac.trim()) ?: return null
    return WakeTarget(canonical, viaId ?: return null)
}

/**
 * Edits how [host] is woken: its MAC, and which of the OTHER saved computers
 * ([relays]) sends the magic packet. The phone cannot broadcast onto the
 * host's LAN itself, so with no second computer paired there is nothing to
 * choose and the dialog says so instead of offering an empty list.
 */
@Composable
internal fun WakeTargetDialog(
    host: SavedPairing,
    relays: List<SavedPairing>,
    onSave: (WakeTarget) -> Unit,
    onClear: () -> Unit,
    onDismiss: () -> Unit,
) {
    var mac by rememberSaveable { mutableStateOf(host.wake?.mac ?: "") }
    // A lone relay is preselected: it is the only possible answer, and asking
    // for it anyway is a tap that cannot change anything.
    var viaId by rememberSaveable { mutableStateOf(host.wake?.viaId ?: relays.singleOrNull()?.id) }
    val target = wakeTargetFrom(mac, viaId)

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Wake-up for ${host.endpointLabel}") },
        text = {
            if (relays.isEmpty()) {
                Text(
                    "Waking a computer needs a second paired computer on the same network that stays on, " +
                        "to send the wake-up packet for it. Pair one first, then come back here.",
                )
            } else {
                WakeTargetFields(
                    mac = mac,
                    onMacChange = { mac = it },
                    relays = relays,
                    viaId = viaId,
                    onViaChange = { viaId = it },
                )
            }
        },
        confirmButton = {
            if (relays.isNotEmpty()) {
                TextButton(onClick = { target?.let(onSave) }, enabled = target != null) { Text("Save") }
            }
        },
        dismissButton = {
            Row {
                if (host.wake != null) TextButton(onClick = onClear) { Text("Clear") }
                TextButton(onClick = onDismiss) { Text("Cancel") }
            }
        },
    )
}

@Composable
private fun WakeTargetFields(
    mac: String,
    onMacChange: (String) -> Unit,
    relays: List<SavedPairing>,
    viaId: String?,
    onViaChange: (String) -> Unit,
) {
    val macInvalid = mac.isNotBlank() && canonicalMac(mac.trim()) == null
    Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
        OutlinedTextField(
            value = mac,
            onValueChange = onMacChange,
            label = { Text("MAC address") },
            placeholder = { Text("aa:bb:cc:dd:ee:ff") },
            singleLine = true,
            isError = macInvalid,
            supportingText = if (macInvalid) ({ Text("Six pairs of hex digits, like aa:bb:cc:dd:ee:ff") }) else null,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Ascii, imeAction = ImeAction.Done),
            modifier = Modifier.fillMaxWidth(),
        )
        Text("Send the wake-up packet from", style = MaterialTheme.typography.labelLarge)
        relays.forEach { relay ->
            Row(
                modifier = Modifier.fillMaxWidth().selectable(
                    selected = relay.id == viaId,
                    onClick = { onViaChange(relay.id) },
                    role = Role.RadioButton,
                ),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                // Selection is handled by the row, so the button itself has no click of its own.
                RadioButton(selected = relay.id == viaId, onClick = null)
                Text(relay.endpointLabel)
            }
        }
    }
}
