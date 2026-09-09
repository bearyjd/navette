package com.greponlabs.navette.protocol

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.long

/**
 * Kotlin mirror of `navette-protocol`'s control-channel wire types
 * (`crates/navette-protocol/src/lib.rs`). The wire shape is not negotiable
 * from this side -- `navetted` defines it -- so [ControlCodec] hand-rolls
 * the flatten/tag layout serde produces instead of fighting
 * kotlinx.serialization's structural polymorphism into matching it, and the
 * round-trip tests in this module's test sources use the exact JSON
 * fixtures the Rust side's own tests assert against.
 */

/** Matches `navette_protocol::WEBSOCKET_SUBPROTOCOL`. */
const val CONTROL_WEBSOCKET_SUBPROTOCOL: String = "navette.v1"

/** `navetted`'s control-channel path, per its `--bind` default in main.rs. */
const val CONTROL_WEBSOCKET_PATH: String = "/v1/ws"

typealias RequestId = Long

@Serializable
sealed interface RequestCommand {
    @Serializable
    @SerialName("list_apps")
    data object ListApps : RequestCommand

    @Serializable
    @SerialName("list_sessions")
    data object ListSessions : RequestCommand

    @Serializable
    @SerialName("run")
    data class Run(
        @SerialName("app_id") val appId: String,
        val name: String? = null,
    ) : RequestCommand

    @Serializable
    @SerialName("kill")
    data class Kill(val session: String) : RequestCommand

    @Serializable
    @SerialName("attach")
    data class Attach(val session: String) : RequestCommand

    @Serializable
    @SerialName("detach")
    data class Detach(val session: String) : RequestCommand
}

@Serializable
sealed interface ResponseResult {
    @Serializable
    @SerialName("apps")
    data class Apps(val apps: List<App>) : ResponseResult

    @Serializable
    @SerialName("sessions")
    data class Sessions(val sessions: List<Session>) : ResponseResult

    @Serializable
    @SerialName("session")
    data class SessionResult(val session: Session) : ResponseResult

    @Serializable
    @SerialName("attach")
    data class AttachResult(val attach: AttachInfo) : ResponseResult

    @Serializable
    @SerialName("ack")
    data object Ack : ResponseResult
}

sealed interface ResponseOutcome {
    data class Ok(val result: ResponseResult) : ResponseOutcome

    data class Error(val error: ApiError) : ResponseOutcome
}

data class Response(val requestId: RequestId, val outcome: ResponseOutcome)

@Serializable
data class App(
    val id: String,
    val name: String,
    val icon: String? = null,
    val categories: List<String> = emptyList(),
    val exec: List<String> = emptyList(),
    val terminal: Boolean = false,
)

@Serializable
data class Session(
    val name: String,
    @SerialName("app_id") val appId: String,
    @SerialName("app_pid") val appPid: Long,
    @SerialName("daemon_pid") val daemonPid: Long,
    @SerialName("wayland_display") val waylandDisplay: String,
    @SerialName("socket_path") val socketPath: String,
    @SerialName("created_at_ms") val createdAtMs: Long,
    @SerialName("last_attached_at_ms") val lastAttachedAtMs: Long? = null,
    @SerialName("client_count") val clientCount: Int = 0,
    val status: SessionStatus,
)

@Serializable
enum class SessionStatus {
    @SerialName("starting") STARTING,
    @SerialName("running") RUNNING,
    @SerialName("failed") FAILED,
    @SerialName("stopped") STOPPED,
}

@Serializable
data class AttachInfo(
    val session: String,
    @SerialName("socket_path") val socketPath: String,
)

@Serializable
data class ApiError(
    val code: ErrorCode,
    val message: String,
)

@Serializable
enum class ErrorCode {
    @SerialName("invalid_request") INVALID_REQUEST,
    @SerialName("not_found") NOT_FOUND,
    @SerialName("already_exists") ALREADY_EXISTS,
    @SerialName("invalid_name") INVALID_NAME,
    @SerialName("process_failed") PROCESS_FAILED,
    @SerialName("unavailable") UNAVAILABLE,
    @SerialName("internal") INTERNAL,
}

class ProtocolException(message: String) : Exception(message)

/**
 * Encodes requests and decodes responses against the exact flat wire shape
 * `navette-protocol` defines: `{request_id, type, ...command fields}` for a
 * request, `{request_id, status, type, ...result fields}` or
 * `{request_id, status:"error", error:{...}}` for a response. Only a
 * client-side codec -- this never needs to decode a `Request` or encode a
 * `Response`, since this app is always the client, never `navetted`.
 */
object ControlCodec {
    // encodeDefaults stays at its library default (false): `Run.name` must
    // be omitted entirely when null, matching navette-protocol's
    // `#[serde(default, skip_serializing_if = "Option::is_none")]` -- not
    // emitted as `"name":null`, which `encodeDefaults = true` would produce.
    val json = Json {
        ignoreUnknownKeys = true
    }

    fun encodeRequest(requestId: RequestId, command: RequestCommand): String {
        val commandJson = json.encodeToJsonElement(RequestCommand.serializer(), command).jsonObject
        val merged = buildJsonObject {
            put("request_id", JsonPrimitive(requestId))
            commandJson.forEach { (key, value) -> put(key, value) }
        }
        return json.encodeToString(JsonObject.serializer(), merged)
    }

    fun decodeResponse(text: String): Response {
        val root = json.parseToJsonElement(text).jsonObject
        val requestId = root["request_id"]?.jsonPrimitive?.long
            ?: throw ProtocolException("response is missing request_id: $text")
        val status = root["status"]?.jsonPrimitive?.content
            ?: throw ProtocolException("response is missing status: $text")
        val outcome = when (status) {
            "ok" -> ResponseOutcome.Ok(json.decodeFromJsonElement(ResponseResult.serializer(), root))
            "error" -> {
                val error = root["error"] ?: throw ProtocolException("error response is missing error: $text")
                ResponseOutcome.Error(json.decodeFromJsonElement(ApiError.serializer(), error))
            }
            else -> throw ProtocolException("unknown response status '$status': $text")
        }
        return Response(requestId, outcome)
    }
}

/**
 * Read helpers so callers can pattern-match on outcome shape once, instead
 * of destructuring `ResponseOutcome`/`ResponseResult` at every call site.
 */
fun Response.appsOrNull(): List<App>? =
    (outcome as? ResponseOutcome.Ok)?.result?.let { (it as? ResponseResult.Apps)?.apps }

fun Response.sessionsOrNull(): List<Session>? =
    (outcome as? ResponseOutcome.Ok)?.result?.let { (it as? ResponseResult.Sessions)?.sessions }

fun Response.errorOrNull(): ApiError? = (outcome as? ResponseOutcome.Error)?.error
