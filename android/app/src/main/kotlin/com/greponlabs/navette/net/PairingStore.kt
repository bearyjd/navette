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
        if (registry.hosts.none { it.id == id }) return registry
        val updated = PairingRegistryCodec.remove(registry, id)
        if (updated.hosts.isEmpty()) clear()
        return updated
    }

    /** Sets or clears how [hostId] is woken; see [PairingRegistryCodec.setWake] for what is rejected. */
    fun setWake(hostId: String, wake: WakeTarget?): PairingRegistry

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

    override fun upsert(pairing: Pairing): PairingRegistry =
        PairingRegistryCodec.upsert(mutableSnapshot(), pairing).also(::writeRegistry)

    override fun select(id: String): PairingRegistry {
        val current = mutableSnapshot()
        require(current.hosts.any { it.id == id })
        return PairingRegistry(current.hosts, id).also(::writeRegistry)
    }

    override fun delete(id: String): PairingRegistry =
        PairingRegistryCodec.remove(mutableSnapshot(), id).also(::writeRegistry)

    override fun setWake(hostId: String, wake: WakeTarget?): PairingRegistry =
        PairingRegistryCodec.setWake(mutableSnapshot(), hostId, wake).also(::writeRegistry)

    override fun clear() = prefs.edit().clear().apply()

    /**
     * The registry a mutation starts from. A snapshot whose `version` is newer
     * than this app's is refused rather than overwritten -- but only when the
     * newer schema added no keys: `ignoreUnknownKeys = false` turns a v3 payload
     * with new fields into Corrupt, not Future, and Corrupt is what
     * [loadRegistry] maps to an empty registry that the next write replaces.
     */
    // TODO: lenient version pre-parse before the strict decode, so a newer
    // payload with unknown keys is recognised as Future rather than Corrupt.
    private fun mutableSnapshot(): PairingRegistry {
        val decoded = registryFromPreferences()
        if (decoded is RegistryDecode.Future) throw IllegalStateException("pairing registry was created by a newer app")
        return (decoded as? RegistryDecode.Valid)?.registry ?: PairingRegistry()
    }

    private fun registryFromPreferences(): RegistryDecode {
        val raw = prefs.getString(REGISTRY_KEY, null)
        if (raw != null) return PairingRegistryCodec.decode(raw)
        val legacy = validatedPairing(
            prefs.getString(LEGACY_HOST_KEY, null) ?: return RegistryDecode.Valid(PairingRegistry()),
            prefs.getInt(LEGACY_PORT_KEY, -1),
            prefs.getString(LEGACY_TOKEN_KEY, null) ?: return RegistryDecode.Valid(PairingRegistry()),
        ) ?: return RegistryDecode.Valid(PairingRegistry())
        val registry = PairingRegistryCodec.upsert(PairingRegistry(), legacy)
        // The current-schema snapshot (v2 today) and removal of the pre-registry
        // legacy fields are one preference edit.
        writeRegistry(registry)
        return RegistryDecode.Valid(registry)
    }

    private fun writeRegistry(registry: PairingRegistry) {
        prefs.edit().putString(REGISTRY_KEY, PairingRegistryCodec.encode(registry))
            .remove(LEGACY_HOST_KEY).remove(LEGACY_PORT_KEY).remove(LEGACY_TOKEN_KEY).apply()
    }

    companion object {
        const val PREFS_NAME = "navette_pairing"

        // A storage slot, not the schema version: the payload's own `version`
        // field is what the codec gates on, and this key has held schema v2
        // since wake targets landed. Renaming it would make every existing
        // install read an empty slot and lose its registry.
        private const val REGISTRY_KEY = "registry_v1"
        private const val LEGACY_HOST_KEY = "host"
        private const val LEGACY_PORT_KEY = "port"
        private const val LEGACY_TOKEN_KEY = "token"
    }
}
