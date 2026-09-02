package com.greponlabs.navette.protocol

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Fixtures are copied verbatim from `crates/navette-protocol/src/lib.rs`'s
 * own `request_fixtures_round_trip` / `response_fixtures_round_trip` /
 * `unknown_fields_are_accepted_for_additive_evolution` tests. This is the
 * actual contract check: if `navetted`'s wire shape ever changes, the Rust
 * suite catches it there, and this suite has to be updated to match --
 * these two are meant to drift apart loudly, not silently.
 */
class ControlProtocolTest {

    private fun parse(text: String): JsonElement = ControlCodec.json.parseToJsonElement(text)

    @Test
    fun `request fixtures encode to the exact flat wire shape`() {
        val cases = listOf(
            1L to RequestCommand.ListApps to """{"request_id":1,"type":"list_apps"}""",
            2L to RequestCommand.ListSessions to """{"request_id":2,"type":"list_sessions"}""",
            3L to RequestCommand.Run("firefox.desktop") to
                """{"request_id":3,"type":"run","app_id":"firefox.desktop"}""",
            4L to RequestCommand.Run("firefox.desktop", "work") to
                """{"request_id":4,"type":"run","app_id":"firefox.desktop","name":"work"}""",
            5L to RequestCommand.Kill("work") to """{"request_id":5,"type":"kill","session":"work"}""",
            6L to RequestCommand.Attach("work") to """{"request_id":6,"type":"attach","session":"work"}""",
            7L to RequestCommand.Detach("work") to """{"request_id":7,"type":"detach","session":"work"}""",
            8L to RequestCommand.SetClipboard("hello") to
                """{"request_id":8,"type":"set_clipboard","text":"hello"}""",
            9L to RequestCommand.GetClipboard to """{"request_id":9,"type":"get_clipboard"}""",
        )

        for ((requestIdAndCommand, expectedJson) in cases) {
            val (requestId, command) = requestIdAndCommand
            val encoded = ControlCodec.encodeRequest(requestId, command)
            assertEquals(parse(expectedJson), parse(encoded))
        }
    }

    @Test
    fun `ok responses decode every ResponseResult variant`() {
        val ok = ControlCodec.decodeResponse(
            """{"request_id":1,"status":"ok","type":"apps","apps":[]}""",
        )
        assertEquals(1L, ok.requestId)
        assertEquals(ResponseOutcome.Ok(ResponseResult.Apps(emptyList())), ok.outcome)

        val sessions = ControlCodec.decodeResponse(
            """{"request_id":2,"status":"ok","type":"sessions","sessions":[]}""",
        )
        assertEquals(ResponseOutcome.Ok(ResponseResult.Sessions(emptyList())), sessions.outcome)

        val ack = ControlCodec.decodeResponse("""{"request_id":3,"status":"ok","type":"ack"}""")
        assertEquals(ResponseOutcome.Ok(ResponseResult.Ack), ack.outcome)

        val clipboard = ControlCodec.decodeResponse(
            """{"request_id":4,"status":"ok","type":"clipboard"}""",
        )
        assertEquals(ResponseOutcome.Ok(ResponseResult.Clipboard(null)), clipboard.outcome)
    }

    @Test
    fun `error responses decode the nested error object`() {
        val response = ControlCodec.decodeResponse(
            """{"request_id":5,"status":"error","error":{"code":"not_found","message":"missing"}}""",
        )
        assertEquals(5L, response.requestId)
        assertEquals(
            ResponseOutcome.Error(ApiError(ErrorCode.NOT_FOUND, "missing")),
            response.outcome,
        )
    }

    @Test
    fun `decoding tolerates unknown fields for additive server evolution`() {
        val response = ControlCodec.decodeResponse(
            """{"request_id":1,"status":"ok","type":"ack","future_field":true}""",
        )
        assertEquals(ResponseOutcome.Ok(ResponseResult.Ack), response.outcome)
    }

    @Test
    fun `a session round trips every field including nullable ones`() {
        val text = """
            {
              "name": "work",
              "app_id": "firefox.desktop",
              "app_pid": 10,
              "daemon_pid": 11,
              "wayland_display": "navette-work",
              "socket_path": "/run/user/1000/navette/work/wprs.sock",
              "created_at_ms": 1700000000000,
              "last_attached_at_ms": 1700000001000,
              "client_count": 2,
              "status": "running"
            }
        """.trimIndent()
        val session = ControlCodec.json.decodeFromString(Session.serializer(), text)
        assertEquals("work", session.name)
        assertEquals("firefox.desktop", session.appId)
        assertEquals(1_700_000_001_000L, session.lastAttachedAtMs)
        assertEquals(SessionStatus.RUNNING, session.status)
        assertTrue(session.clientCount == 2)
    }
}
