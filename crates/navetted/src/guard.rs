use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use navette_auth::AuthToken;

/// Refuses any request a browser originated.
///
/// navette has no browser client, so this is a blanket rejection rather than a
/// policy with an allowlist to maintain. It is load-bearing: browsers do not
/// apply CORS preflight to WebSocket handshakes -- they open the connection and
/// leave rejection to the server -- so without this, any page the user visits
/// can attach to `/v1/sessions/{s}/media`, receive the screen, and inject input
/// while navetted runs on loopback.
///
/// Presence is the test, not the value: `null` and the empty string are what a
/// sandboxed iframe and some privacy tools send, and all of them are browsers.
pub async fn reject_browser_origin(request: Request, next: Next) -> Response {
    if request.headers().contains_key(axum::http::header::ORIGIN) {
        return (
            StatusCode::FORBIDDEN,
            "requests from browsers are not accepted",
        )
            .into_response();
    }
    next.run(request).await
}

/// Requires `Authorization: Bearer <token>` on every route.
///
/// There is no loopback exemption. An earlier design proposed one, reasoning
/// that a loopback TCP port is equivalent to a 0700 Unix socket -- it is not. A
/// 0700 socket is unreachable from a web page; a loopback port is not.
pub async fn authenticate(
    State(token): State<Arc<AuthToken>>,
    request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    // A bare 401 with no detail: saying *why* it failed tells an attacker
    // whether a token was recognised at all.
    match presented {
        Some(presented) if token.matches(presented) => next.run(request).await,
        _ => (StatusCode::UNAUTHORIZED, "").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::api::tests_support::test_router;

    #[tokio::test]
    async fn rejects_any_request_carrying_an_origin_header() {
        // Browsers do not preflight WebSocket handshakes: they open the socket
        // and leave rejection to us. Without this, any visited page can attach
        // to a session and read the screen.
        for path in ["/healthz", "/v1/ws", "/v1/sessions/work/media"] {
            let router = test_router();
            let response = router
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("Origin", "https://evil.example")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{path} must refuse a request with an Origin header"
            );
        }
    }

    #[tokio::test]
    async fn allows_a_request_with_no_origin_header() {
        let (router, token) = crate::api::tests_support::test_router_with_token();
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .header("Authorization", format!("Bearer {}", token.render()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn rejects_an_empty_origin_header() {
        // "null" and "" are what a sandboxed iframe and some privacy tools
        // send. Presence is the rule, not the value.
        let router = test_router();
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .header("Origin", "")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn rejects_a_request_with_no_authorization_header() {
        for path in ["/healthz", "/v1/ws", "/v1/sessions/work/media"] {
            let router = test_router();
            let response = router
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{path} must require a token"
            );
        }
    }

    #[tokio::test]
    async fn rejects_a_wrong_token() {
        let router = test_router();
        let wrong = navette_auth::AuthToken::generate().render();
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .header("Authorization", format!("Bearer {wrong}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn accepts_the_configured_token() {
        let (router, token) = crate::api::tests_support::test_router_with_token();
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .header("Authorization", format!("Bearer {}", token.render()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn origin_is_refused_before_the_token_is_checked() {
        // A browser presenting a stolen token must still get 403, not 401: the
        // origin rule is the outer gate and its failure must not be maskable by
        // supplying a valid credential.
        let (router, token) = crate::api::tests_support::test_router_with_token();
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .header("Origin", "https://evil.example")
                    .header("Authorization", format!("Bearer {}", token.render()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
