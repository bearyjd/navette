package com.greponlabs.navette.net

import android.content.Context
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey

interface PairingStore {
    fun load(): Pairing?
    fun save(pairing: Pairing)
    fun clear()
}

/**
 * Single-slot: saving replaces whatever was paired, host and token together.
 * That is the honest behaviour for one slot, and the alternative a user might
 * expect -- accumulating hosts -- is what M4 adds and this does not.
 */
class EncryptedPairingStore(context: Context) : PairingStore {
    private val prefs by lazy {
        val key = MasterKey.Builder(context)
            .setKeyScheme(MasterKey.KeyScheme.AES256_GCM)
            .build()
        EncryptedSharedPreferences.create(
            context,
            PREFS_NAME,
            key,
            EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
            EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
        )
    }

    override fun load(): Pairing? {
        val host = prefs.getString("host", null) ?: return null
        val token = prefs.getString("token", null) ?: return null
        val port = prefs.getInt("port", -1).takeIf { it > 0 } ?: return null
        return Pairing(host, port, token)
    }

    override fun save(pairing: Pairing) {
        prefs.edit()
            .putString("host", pairing.host)
            .putInt("port", pairing.port)
            .putString("token", pairing.token)
            .apply()
    }

    override fun clear() = prefs.edit().clear().apply()

    companion object {
        /**
         * Also named in `res/xml/backup_rules.xml` and
         * `res/xml/data_extraction_rules.xml`, which exclude `$PREFS_NAME.xml`
         * from backup: this store is keystore-encrypted, and a backup carries
         * the ciphertext but not the non-exportable master key, so a restored
         * file can never be decrypted and every touch of the store throws.
         *
         * Android resource XML cannot reference a Kotlin constant, so the name
         * genuinely lives in three files. `PairingBackupRulesTest` reads the
         * two XMLs and asserts they exclude this exact value, because a rename
         * here that silently unprotects the store would present as "pairing
         * broken after switching phones" and nothing else.
         */
        const val PREFS_NAME = "navette_pairing"
    }
}
