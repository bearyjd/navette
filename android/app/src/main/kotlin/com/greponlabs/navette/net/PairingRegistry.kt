package com.greponlabs.navette.net

import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import java.util.UUID

/**
 * How to wake a saved host that is asleep. The phone cannot broadcast onto the
 * host's LAN itself, so [viaId] names ANOTHER saved pairing -- an always-on
 * daemon on that LAN -- that sends the magic packet for [mac] on its behalf.
 * [mac] is canonical: lowercase, colon-separated (see [canonicalMac]).
 */
data class WakeTarget(val mac: String, val viaId: String)

/** A saved endpoint label is derived; neither it nor the token is user-editable metadata. */
data class SavedPairing(val id: String, val pairing: Pairing, val wake: WakeTarget? = null) {
    val endpointLabel: String get() = if (pairing.host.contains(':')) "[${pairing.host}]:${pairing.port}" else "${pairing.host}:${pairing.port}"
    override fun toString(): String = "SavedPairing(id=$id, endpoint=$endpointLabel, pairing=$pairing, wake=$wake)"
}

data class PairingRegistry(val hosts: List<SavedPairing> = emptyList(), val activeId: String? = null) {
    val active: SavedPairing? get() = hosts.firstOrNull { it.id == activeId }
}

@Serializable
private data class RegistryWire(val version: Int, val activeId: String? = null, val hosts: List<HostWire> = emptyList())

/** `mac` and `wakeViaId` arrived together in version 2; [PairingRegistryCodec.decode] reads one without the other as no wake target. */
@Serializable
private data class HostWire(val id: String, val host: String, val port: Int, val token: String, val mac: String? = null, val wakeViaId: String? = null)

internal sealed interface RegistryDecode {
    data class Valid(val registry: PairingRegistry) : RegistryDecode
    data object Corrupt : RegistryDecode
    data object Future : RegistryDecode
}

/** Pure codec so malformed storage can be tested without Android keystore plumbing. */
internal object PairingRegistryCodec {
    private const val VERSION = 2
    private const val FIRST_VERSION_WITH_WAKE = 2
    private val json = Json { ignoreUnknownKeys = false }

    fun encode(registry: PairingRegistry): String {
        val ids = registry.hosts.map { it.id }.toSet()
        require(ids.size == registry.hosts.size)
        require(registry.activeId == null || registry.activeId in ids)
        val hosts = registry.hosts.map { saved ->
            val pairing = validatedPairing(saved.pairing.host, saved.pairing.port, saved.pairing.token)
                ?: error("attempted to persist an invalid pairing")
            val wake = saved.wake?.let { validatedWake(saved.id, it.mac, it.viaId, ids) ?: error("attempted to persist an invalid wake target") }
            HostWire(saved.id, pairing.host, pairing.port, pairing.token, wake?.mac, wake?.viaId)
        }
        return json.encodeToString(RegistryWire.serializer(), RegistryWire(VERSION, registry.activeId, hosts))
    }

    /**
     * Fails closed on anything pairing-level -- an id, host, port or token
     * that cannot be trusted, or an `activeId` naming no host -- but degrades
     * on wake metadata: a MAC without a relay (or vice versa), a relay that is
     * not in the registry or is the host itself, or a MAC that does not parse,
     * decodes as "no wake target" and the host is kept. `loadRegistry` maps
     * Corrupt to an empty registry and the next write overwrites the blob, so
     * a Corrupt over recoverable metadata would cost every host and token.
     * [encode] still refuses to write such a target; this is the reader's half.
     */
    fun decode(raw: String): RegistryDecode {
        val wire = runCatching { json.decodeFromString(RegistryWire.serializer(), raw) }.getOrNull() ?: return RegistryDecode.Corrupt
        if (wire.version > VERSION) return RegistryDecode.Future
        val ids = wire.hosts.map { it.id }.toSet()
        if (wire.version !in 1..VERSION || ids.size != wire.hosts.size || wire.hosts.any { it.id.isBlank() }) return RegistryDecode.Corrupt
        // No shipped version 1 writer ever emitted wake fields, so their presence is corruption, not an early adopter.
        if (wire.version < FIRST_VERSION_WITH_WAKE && wire.hosts.any { it.mac != null || it.wakeViaId != null }) return RegistryDecode.Corrupt
        val hosts = wire.hosts.map { item ->
            val pairing = validatedPairing(item.host, item.port, item.token) ?: return RegistryDecode.Corrupt
            val wake = item.mac?.let { mac -> item.wakeViaId?.let { via -> validatedWake(item.id, mac, via, ids) } }
            SavedPairing(item.id, pairing, wake)
        }
        if (wire.activeId != null && wire.activeId !in ids) return RegistryDecode.Corrupt
        return RegistryDecode.Valid(PairingRegistry(hosts, wire.activeId))
    }

    /** Re-pairing an endpoint rotates its token; the wake target belongs to the machine, not the token, so it stays. */
    fun upsert(registry: PairingRegistry, pairing: Pairing): PairingRegistry {
        val valid = validatedPairing(pairing.host, pairing.port, pairing.token) ?: throw IllegalArgumentException("invalid pairing")
        val existing = registry.hosts.firstOrNull { it.pairing.host == valid.host && it.pairing.port == valid.port }
        val saved = SavedPairing(existing?.id ?: UUID.randomUUID().toString(), valid, existing?.wake)
        val hosts = registry.hosts.filterNot { it.id == saved.id } + saved
        return PairingRegistry(hosts, saved.id)
    }

    /**
     * Removes [id], and clears every wake target that relayed through it: a
     * dangling `viaId` is exactly what [decode] refuses, so leaving one behind
     * would make the next launch read the whole registry as corrupt.
     */
    fun remove(registry: PairingRegistry, id: String): PairingRegistry {
        val hosts = registry.hosts
            .filterNot { it.id == id }
            .map { saved -> if (saved.wake?.viaId == id) saved.copy(wake = null) else saved }
        return PairingRegistry(hosts, registry.activeId?.takeIf { it != id })
    }

    /** Sets or clears (`wake == null`) how [hostId] is woken. [WakeTarget.mac] is canonicalized on the way in. */
    fun setWake(registry: PairingRegistry, hostId: String, wake: WakeTarget?): PairingRegistry {
        require(registry.hosts.any { it.id == hostId }) { "unknown host" }
        val validated =
            wake?.let {
                val mac = canonicalMac(it.mac) ?: throw IllegalArgumentException("invalid MAC address")
                require(it.viaId != hostId) { "a host cannot wake itself" }
                require(registry.hosts.any { saved -> saved.id == it.viaId }) { "unknown relay host" }
                WakeTarget(mac, it.viaId)
            }
        return registry.copy(hosts = registry.hosts.map { if (it.id == hostId) it.copy(wake = validated) else it })
    }

    private fun validatedWake(hostId: String, mac: String, viaId: String, ids: Set<String>): WakeTarget? {
        val canonical = canonicalMac(mac) ?: return null
        if (viaId == hostId || viaId !in ids) return null
        return WakeTarget(canonical, viaId)
    }
}
