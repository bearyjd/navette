package com.greponlabs.navette.ui.hosts

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import com.greponlabs.navette.net.PairingRegistry
import com.greponlabs.navette.net.SavedPairing
import com.greponlabs.navette.net.WakeTarget

/**
 * [onSetWake] sets, or with `null` clears, how a saved host is woken -- see
 * [WakeTargetDialog]. [snackbarMessage] and [onSnackbarDismissed] follow
 * `DrawerScreen`'s contract: this is where a wake-up setting that failed to
 * save is reported, so the notice has to be visible here rather than the next
 * time the drawer happens to show.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun HostListScreen(
    registry: PairingRegistry,
    snackbarMessage: String?,
    onSelect: (String) -> Unit,
    onDelete: (String) -> Unit,
    onSetWake: (hostId: String, wake: WakeTarget?) -> Unit,
    onAdd: () -> Unit,
    onBack: () -> Unit,
    onSnackbarDismissed: (shown: String) -> Unit,
) {
    val snackbarHostState = remember { SnackbarHostState() }
    LaunchedEffect(snackbarMessage) {
        if (snackbarMessage != null) {
            snackbarHostState.showSnackbar(snackbarMessage)
            onSnackbarDismissed(snackbarMessage)
        }
    }
    // The id, not the SavedPairing: the registry can change underneath an open
    // dialog (a save landing, a delete), and the dialog should show what is
    // stored now, or close if its host is gone.
    var editingWakeFor by rememberSaveable { mutableStateOf<String?>(null) }
    val editing = registry.hosts.firstOrNull { it.id == editingWakeFor }
    if (editing != null) {
        WakeTargetDialog(
            host = editing,
            relays = registry.hosts.filterNot { it.id == editing.id },
            onSave = { target ->
                onSetWake(editing.id, target)
                editingWakeFor = null
            },
            onClear = {
                onSetWake(editing.id, null)
                editingWakeFor = null
            },
            onDismiss = { editingWakeFor = null },
        )
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Your computers") },
                navigationIcon = { if (registry.active != null) TextButton(onClick = onBack) { Text("Back") } },
                actions = { TextButton(onClick = onAdd) { Text("Add") } },
            )
        },
        snackbarHost = { SnackbarHost(snackbarHostState) },
    ) { padding ->
        Column(
            modifier = Modifier.fillMaxSize().padding(padding).padding(20.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            if (registry.hosts.isEmpty()) {
                Text("No paired computers", style = MaterialTheme.typography.headlineSmall)
                Text("Add a pairing code to connect to your Linux desktop.")
                Button(onClick = onAdd) { Text("Add computer") }
            } else {
                registry.hosts.forEach { saved ->
                    HostCard(
                        saved = saved,
                        registry = registry,
                        onSelect = { onSelect(saved.id) },
                        onDelete = { onDelete(saved.id) },
                        onEditWake = { editingWakeFor = saved.id },
                    )
                }
            }
        }
    }
}

@Composable
private fun HostCard(
    saved: SavedPairing,
    registry: PairingRegistry,
    onSelect: () -> Unit,
    onDelete: () -> Unit,
    onEditWake: () -> Unit,
) {
    Card(
        modifier = Modifier.fillMaxWidth().semantics {
            contentDescription = "Select ${saved.endpointLabel}"
            role = Role.Button
        }.clickable(onClick = onSelect),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(16.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(modifier = Modifier.weight(1f)) {
                Text(saved.endpointLabel, fontWeight = FontWeight.SemiBold)
                if (saved.id == registry.activeId) Text("Active", color = MaterialTheme.colorScheme.primary)
                // The relay's label, not its id: an id that no longer resolves
                // cannot be stored (the codec refuses it), so this is total.
                val relay = saved.wake?.let { wake -> registry.hosts.firstOrNull { it.id == wake.viaId } }
                if (relay != null) Text("Wakes via ${relay.endpointLabel}", style = MaterialTheme.typography.bodySmall)
            }
            Column(horizontalAlignment = Alignment.End) {
                TextButton(onClick = onEditWake) { Text("Edit wake-up") }
                TextButton(onClick = onDelete) { Text("Delete") }
            }
        }
    }
}
