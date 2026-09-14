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
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import com.greponlabs.navette.net.PairingRegistry

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun HostListScreen(
    registry: PairingRegistry,
    onSelect: (String) -> Unit,
    onDelete: (String) -> Unit,
    onAdd: () -> Unit,
    onBack: () -> Unit,
) {
    Scaffold(topBar = {
        TopAppBar(
            title = { Text("Your computers") },
            navigationIcon = { if (registry.active != null) TextButton(onClick = onBack) { Text("Back") } },
            actions = { TextButton(onClick = onAdd) { Text("Add") } },
        )
    }) { padding ->
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
                    Card(
                        modifier = Modifier.fillMaxWidth().semantics {
                            contentDescription = "Select ${saved.endpointLabel}"
                            role = Role.Button
                        }.clickable { onSelect(saved.id) },
                    ) {
                        Row(
                            modifier = Modifier.fillMaxWidth().padding(16.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Column(modifier = Modifier.weight(1f)) {
                                Text(saved.endpointLabel, fontWeight = FontWeight.SemiBold)
                                if (saved.id == registry.activeId) Text("Active", color = MaterialTheme.colorScheme.primary)
                            }
                            TextButton(onClick = { onDelete(saved.id) }) { Text("Delete") }
                        }
                    }
                }
            }
        }
    }
}
