package com.greponlabs.navette.net

import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

sealed interface WakeResult {
    /** The relay accepted the request and put a magic packet on its LAN. Whether the target boots is not knowable from here. */
    data object Sent : WakeResult

    /** The relay answered with something other than 204: 400 malformed, 401 bad token, 5xx it could not send. */
    data class Rejected(val code: Int) : WakeResult

    /** No HTTP answer at all -- the relay itself is down, unreachable, or timed out. */
    data object Unreachable : WakeResult
}

/**
 * What [AppViewModel][com.greponlabs.navette.ui.AppViewModel] needs to wake a
 * host. Exists so tests can inject a fake instead of standing up real
 * networking -- see `AppViewModelTest`.
 */
interface WakeTransport {
    suspend fun wake(via: Pairing, mac: String): WakeResult
}

/** Exactly the body `navetted`'s `POST /v1/wake` takes; `broadcast` and `port` are optional there and deliberately never sent. */
@Serializable
private data class WakeRequestWire(val mac: String)

/**
 * Asks the always-on daemon at [via] to broadcast a magic packet for [mac] on
 * its own LAN. One request, no retry: a magic packet is fire-and-forget on the
 * relay's side too, and the user has a Retry of their own once the host has
 * had a minute to boot.
 */
internal class HttpWakeTransport : WakeTransport {
    override suspend fun wake(via: Pairing, mac: String): WakeResult =
        withContext(Dispatchers.IO) {
            val body = Json.encodeToString(WakeRequestWire.serializer(), WakeRequestWire(mac)).toByteArray(Charsets.UTF_8)
            try {
                val connection = URL(wakeUrl(via.host, via.port)).openConnection() as HttpURLConnection
                try {
                    connection.requestMethod = "POST"
                    // As navette-cli's http_client: a redirect must not turn an
                    // authenticated request into one to another authority, and
                    // the API never needs redirects.
                    connection.instanceFollowRedirects = false
                    // The token never reaches a log: nothing below prints the connection or its headers.
                    connection.setRequestProperty("Authorization", "Bearer ${via.token}")
                    connection.setRequestProperty("Content-Type", "application/json")
                    connection.connectTimeout = CONNECT_TIMEOUT_MS
                    connection.readTimeout = READ_TIMEOUT_MS
                    connection.doOutput = true
                    connection.outputStream.use { it.write(body) }
                    when (val code = connection.responseCode) {
                        HttpURLConnection.HTTP_NO_CONTENT -> WakeResult.Sent
                        else -> WakeResult.Rejected(code)
                    }
                } finally {
                    connection.disconnect()
                }
            } catch (error: IOException) {
                WakeResult.Unreachable
            }
        }

    private companion object {
        const val CONNECT_TIMEOUT_MS = 10_000
        const val READ_TIMEOUT_MS = 15_000
    }
}

/** The relay's wake endpoint, with the same IPv6 bracketing as the WebSocket URLs. */
internal fun wakeUrl(host: String, port: Int): String = "http://${formatAuthorityHost(host)}:$port/v1/wake"
