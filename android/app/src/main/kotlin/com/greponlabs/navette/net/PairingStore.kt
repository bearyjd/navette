package com.greponlabs.navette.net

import android.content.Context
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey

/** Encrypted host address book; [load] and [save] address its active endpoint. */
interface PairingStore {
    fun load(): Pairing?
    fun save(pairing: Pairing)
    fun clear()

    fun loadRegistry(): PairingRegistry =
        load()?.let { PairingRegistry(listOf(SavedPairing(LEGACY_ID, it)), LEGACY_ID) } ?: PairingRegistry()

    fun upsert(pairing: Pairing): PairingRegistry {
        save(pairing)
        return loadRegistry()
    }

    fun select(id: String): PairingRegistry {
        val registry = loadRegistry()
        require(registry.hosts.any { it.id == id })
        return PairingRegistry(registry.hosts, id)
    }

    fun delete(id: String): PairingRegistry {
        val registry = loadRegistry()
        val hosts = registry.hosts.filterNot { it.id == id }
        if (hosts.size == registry.hosts.size) return registry
        if (hosts.isEmpty()) clear()
        return PairingRegistry(hosts, registry.activeId?.takeIf { it != id })
    }

    companion object { const val LEGACY_ID = "legacy" }
}

class EncryptedPairingStore(context: Context) : PairingStore {
    private val prefs by lazy {
        val key = MasterKey.Builder(context).setKeyScheme(MasterKey.KeyScheme.AES256_GCM).build()
        EncryptedSharedPreferences.create(
            context, PREFS_NAME, key,
            EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
            EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
        )
    }

    override fun load(): Pairing? = loadRegistry().active?.pairing
    override fun save(pairing: Pairing) { upsert(pairing) }

    override fun loadRegistry(): PairingRegistry =
        when (val decoded = registryFromPreferences()) {
            is RegistryDecode.Valid -> decoded.registry
            // Never resurrect legacy keys after a current corrupt snapshot.
            RegistryDecode.Corrupt, RegistryDecode.Future -> PairingRegistry()
        }

    override fun upsert(pairing: Pairing): PairingRegistry {
        val decoded = registryFromPreferences()
        if (decoded is RegistryDecode.Future) throw IllegalStateException("pairing registry was created by a newer app")
        val updated = PairingRegistryCodec.upsert((decoded as? RegistryDecode.Valid)?.registry ?: PairingRegistry(), pairing)
        writeRegistry(updated)
        return updated
    }

    override fun select(id: String): PairingRegistry {
        val decoded = registryFromPreferences()
        if (decoded is RegistryDecode.Future) throw IllegalStateException("pairing registry was created by a newer app")
        val current = (decoded as? RegistryDecode.Valid)?.registry ?: PairingRegistry()
        require(current.hosts.any { it.id == id })
        return PairingRegistry(current.hosts, id).also(::writeRegistry)
    }

    override fun delete(id: String): PairingRegistry {
        val decoded = registryFromPreferences()
        if (decoded is RegistryDecode.Future) throw IllegalStateException("pairing registry was created by a newer app")
        val current = (decoded as? RegistryDecode.Valid)?.registry ?: PairingRegistry()
        val updated = PairingRegistry(current.hosts.filterNot { it.id == id }, current.activeId?.takeIf { it != id })
        writeRegistry(updated)
        return updated
    }

    override fun clear() = prefs.edit().clear().apply()

    private fun registryFromPreferences(): RegistryDecode {
        val raw = prefs.getString(REGISTRY_KEY, null)
        if (raw != null) return PairingRegistryCodec.decode(raw)
        val legacy = validatedPairing(
            prefs.getString(LEGACY_HOST_KEY, null) ?: return RegistryDecode.Valid(PairingRegistry()),
            prefs.getInt(LEGACY_PORT_KEY, -1),
            prefs.getString(LEGACY_TOKEN_KEY, null) ?: return RegistryDecode.Valid(PairingRegistry()),
        ) ?: return RegistryDecode.Valid(PairingRegistry())
        val registry = PairingRegistryCodec.upsert(PairingRegistry(), legacy)
        // The v1 snapshot and removal of legacy fields are one preference edit.
        writeRegistry(registry)
        return RegistryDecode.Valid(registry)
    }

    private fun writeRegistry(registry: PairingRegistry) {
        prefs.edit().putString(REGISTRY_KEY, PairingRegistryCodec.encode(registry))
            .remove(LEGACY_HOST_KEY).remove(LEGACY_PORT_KEY).remove(LEGACY_TOKEN_KEY).apply()
    }

    companion object {
        const val PREFS_NAME = "navette_pairing"
        private const val REGISTRY_KEY = "registry_v1"
        private const val LEGACY_HOST_KEY = "host"
        private const val LEGACY_PORT_KEY = "port"
        private const val LEGACY_TOKEN_KEY = "token"
    }
}
