use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

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
        let router = test_router();
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/healthz")
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
}
