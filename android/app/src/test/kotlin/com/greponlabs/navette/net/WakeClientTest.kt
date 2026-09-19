package com.greponlabs.navette.net

import java.util.concurrent.TimeUnit
import kotlinx.coroutines.test.runTest
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Before
import org.junit.Test

/**
 * The wire contract with `navetted`'s `POST /v1/wake`, pinned end to end
 * against a real socket: the daemon side is being written against exactly
 * this request shape, so a drift in the path, header or body is a 400 or a
 * 401 on a real phone, not a compile error.
 */
class WakeClientTest {
    private val token = "ABCD1234ABCD1234ABCD1234"
    private val mac = "aa:bb:cc:dd:ee:ff"
    private lateinit var server: MockWebServer

    @Before
    fun setUp() {
        server = MockWebServer()
        server.start()
    }

    @After
    fun tearDown() {
        runCatching { server.shutdown() }
    }

    private fun relay() = Pairing(server.hostName, server.port, token)

    @Test
    fun `wake url follows the WebSocket URLs' IPv6 bracketing`() {
        assertEquals("http://tower:9417/v1/wake", wakeUrl("tower", 9417))
        assertEquals("http://192.168.1.5:19417/v1/wake", wakeUrl("192.168.1.5", 19417))
        assertEquals("http://[fd7a:115c:a1e0::1]:9417/v1/wake", wakeUrl("fd7a:115c:a1e0::1", 9417))
    }

    @Test
    fun `a 204 is Sent, and the request is exactly what the daemon expects`() =
        runTest {
            server.enqueue(MockResponse().setResponseCode(204))

            assertEquals(WakeResult.Sent, HttpWakeTransport().wake(relay(), mac))

            val request = server.takeRequest(5, TimeUnit.SECONDS) ?: error("no request reached the relay")
            assertEquals("POST", request.method)
            assertEquals("/v1/wake", request.path)
            assertEquals("Bearer $token", request.getHeader("Authorization"))
            assertEquals("application/json", request.getHeader("Content-Type"))
            // `broadcast` and `port` are optional daemon-side and deliberately never sent.
            assertEquals("""{"mac":"aa:bb:cc:dd:ee:ff"}""", request.body.readUtf8())
        }

    @Test
    fun `any other status is Rejected with that code`() =
        runTest {
            listOf(400, 401, 404, 500, 503).forEach { code ->
                server.enqueue(MockResponse().setResponseCode(code).setBody("""{"error":"nope"}"""))
                assertEquals(WakeResult.Rejected(code), HttpWakeTransport().wake(relay(), mac))
            }
        }

    @Test
    fun `a relay that does not answer is Unreachable`() =
        runTest {
            val via = relay()
            server.shutdown()

            assertEquals(WakeResult.Unreachable, HttpWakeTransport().wake(via, mac))
        }

    @Test
    fun `a redirect is not followed, so the bearer never reaches another authority`() =
        runTest {
            // Same rule as the CLI's http_client: the API never needs
            // redirects, and following one would replay the token to
            // whatever the Location header names.
            server.enqueue(MockResponse().setResponseCode(302).setHeader("Location", server.url("/elsewhere").toString()))
            server.enqueue(MockResponse().setResponseCode(204)) // what a follower would land on

            assertEquals(WakeResult.Rejected(302), HttpWakeTransport().wake(relay(), mac))
            assertEquals("exactly one request may reach the relay", 1, server.requestCount)
        }

    @Test
    fun `a rejection carries the status and nothing from the request`() =
        runTest {
            // WakeResult is what ends up in UI state and therefore in any log
            // of it; the token must not ride along on the failure path.
            server.enqueue(MockResponse().setResponseCode(401))
            val result = HttpWakeTransport().wake(relay(), mac)
            assertEquals(WakeResult.Rejected(401), result)
            assertFalse(result.toString().contains(token))
        }
}
