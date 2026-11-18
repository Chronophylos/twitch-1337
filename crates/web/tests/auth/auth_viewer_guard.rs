use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt as _;

#[tokio::test]
async fn viewer_method_guard_admits_only_safe_methods() {
    let cases = [
        (Method::GET, StatusCode::OK),
        (Method::HEAD, StatusCode::OK),
        (Method::POST, StatusCode::METHOD_NOT_ALLOWED),
        (Method::DELETE, StatusCode::METHOD_NOT_ALLOWED),
    ];
    for (method, expected) in cases {
        let app = Router::new()
            .route("/x", axum::routing::any(|| async { "ok" }))
            .layer(axum::middleware::from_fn(
                twitch_1337_web::auth::viewer_method_guard,
            ));

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/x")
                    .method(method.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), expected, "{method}");
    }
}
