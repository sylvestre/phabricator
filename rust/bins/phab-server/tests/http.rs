//! The HTTP routes of the service.

use std::sync::Arc;

use axum::body::Body;

use axum::body;
use axum::http::{Request, StatusCode};
use phab_conduit::{Context, Envelope, Registry};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use phab_server::{AppState, FALLBACK_HEADER};
use tower::ServiceExt;

async fn app() -> Option<(TestDatabase, axum::Router)> {
    let test_db = TestDatabase::create(&["conduit"]).await?;
    let state = Arc::new(AppState {
        context: Context {
            database: test_db.database().clone(),
            config: Config::default(),
            hostname: "web1".to_owned(),
        },
        registry: Registry::default(),
        remarkup: phab_web::remarkup::new_engine(&Config::default()),
        php: None,
        shadow: None,
    });
    Some((test_db, phab_server::router(state)))
}

#[tokio::test]
async fn status_reports_the_database() {
    let Some((test_db, app)) = app().await else {
        return;
    };
    let response = app
        .oneshot(Request::get("/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["status"], "ok");
    test_db.drop_databases().await;
}

#[tokio::test]
async fn unknown_methods_go_back_to_php() {
    let Some((test_db, app)) = app().await else {
        return;
    };
    let response = app
        .oneshot(
            Request::post("/api/maniphest.search")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("api.token=x"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
    assert_eq!(response.headers()[FALLBACK_HEADER], "method not ported");
    test_db.drop_databases().await;
}

#[tokio::test]
async fn envelopes_have_php_headers() {
    let response = phab_server::envelope_response(&Envelope::success(serde_json::json!("web1")));
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers();
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(headers["access-control-allow-origin"], "*");
    assert_eq!(headers["x-conduit-capabilities"], "gzip");
    let body = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        &body[..],
        br#"{"result":"web1","error_code":null,"error_info":null}"#
    );
}
