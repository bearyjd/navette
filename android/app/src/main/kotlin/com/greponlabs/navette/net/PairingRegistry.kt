package com.greponlabs.navette.net

import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import java.util.UUID

/** A saved endpoint label is derived; neither it nor the token is user-editable metadata. */
data class SavedPairing(val id: String, val pairing: Pairing) {
    val endpointLabel: String get() = if (pairing.host.contains(':')) "[${pairing.host}]:${pairing.port}" else "${pairing.host}:${pairing.port}"
    override fun toString(): String = "SavedPairing(id=$id, endpoint=$endpointLabel, pairing=$pairing)"
}

data class PairingRegistry(val hosts: List<SavedPairing> = emptyList(), val activeId: String? = null) {
    val active: SavedPairing? get() = hosts.firstOrNull { it.id == activeId }
}

@Serializable
private data class RegistryWire(val version: Int, val activeId: String? = null, val hosts: List<HostWire> = emptyList())

@Serializable
private data class HostWire(val id: String, val host: String, val port: Int, val token: String)

internal sealed interface RegistryDecode {
    data class Valid(val registry: PairingRegistry) : RegistryDecode
    data object Corrupt : RegistryDecode
    data object Future : RegistryDecode
}

/** Pure codec so malformed storage can be tested without Android keystore plumbing. */
internal object PairingRegistryCodec {
    private const val VERSION = 1
    private val json = Json { ignoreUnknownKeys = false }

    fun encode(registry: PairingRegistry): String {
        require(registry.hosts.map { it.id }.distinct().size == registry.hosts.size)
        require(registry.activeId == null || registry.hosts.any { it.id == registry.activeId })
        val hosts = registry.hosts.map { saved ->
            val pairing = validatedPairing(saved.pairing.host, saved.pairing.port, saved.pairing.token)
                ?: error("attempted to persist an invalid pairing")
            HostWire(saved.id, pairing.host, pairing.port, pairing.token)
        }
        return json.encodeToString(RegistryWire.serializer(), RegistryWire(VERSION, registry.activeId, hosts))
    }

    fun decode(raw: String): RegistryDecode {
        val wire = runCatching { json.decodeFromString(RegistryWire.serializer(), raw) }.getOrNull() ?: return RegistryDecode.Corrupt
        if (wire.version > VERSION) return RegistryDecode.Future
        if (wire.version != VERSION || wire.hosts.map { it.id }.distinct().size != wire.hosts.size || wire.hosts.any { it.id.isBlank() }) return RegistryDecode.Corrupt
        val hosts = wire.hosts.map { item ->
            val pairing = validatedPairing(item.host, item.port, item.token) ?: return RegistryDecode.Corrupt
            SavedPairing(item.id, pairing)
        }
        if (wire.activeId != null && hosts.none { it.id == wire.activeId }) return RegistryDecode.Corrupt
        return RegistryDecode.Valid(PairingRegistry(hosts, wire.activeId))
    }

    fun upsert(registry: PairingRegistry, pairing: Pairing): PairingRegistry {
        val valid = validatedPairing(pairing.host, pairing.port, pairing.token) ?: throw IllegalArgumentException("invalid pairing")
        val existing = registry.hosts.firstOrNull { it.pairing.host == valid.host && it.pairing.port == valid.port }
        val saved = SavedPairing(existing?.id ?: UUID.randomUUID().toString(), valid)
        val hosts = registry.hosts.filterNot { it.id == saved.id } + saved
        return PairingRegistry(hosts, saved.id)
    }
}
