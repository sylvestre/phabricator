//! Shadow mode against a fake PHP server.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::{Router, body, routing};
use phab_conduit::{ApiRequest, Application, Context, Method, MethodFuture, Registry};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use phab_server::AppState;
use phab_server::shadow::Shadow;
use serde_json::{Value, json};
use tower::ServiceExt;

struct Answer(&'static str, Value);

impl Method for Answer {
    fn name(&self) -> &'static str {
        self.0
    }

    fn params(&self) -> &'static [&'static str] {
        &[]
    }

    fn requires_authentication(&self) -> bool {
        false
    }

    fn application(&self) -> Option<Application> {
        None
    }

    fn execute<'a>(&'a self, _request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move { Ok(self.1.clone()) })
    }
}

/// A stand-in for PHP which answers `{"result": 42}` and checks it was
/// called with the skip header.
async fn fake_php() -> String {
    async fn answer(headers: HeaderMap) -> (StatusCode, [(&'static str, &'static str); 1], String) {
        let result = if headers.contains_key("x-phab-skip-rust") {
            42
        } else {
            -1
        };
        (
            StatusCode::OK,
            [("content-type", "application/json")],
            format!(r#"{{"result":{result},"error_code":null,"error_info":null}}"#),
        )
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/api/{method}", routing::post(answer)),
        )
        .await
        .unwrap();
    });
    format!("http://{address}")
}

#[tokio::test]
async fn php_answers_and_rust_is_compared() {
    let Some(test_db) = TestDatabase::create(&["conduit"]).await else {
        return;
    };
    let mut registry = Registry::default();
    registry.register(Answer("test.same", json!(42)));
    registry.register(Answer("test.different", json!("42")));
    let shadow = Shadow::new(
        ["test.same", "test.different", "test.missing"].map(str::to_owned),
        fake_php().await,
    );
    let state = Arc::new(AppState {
        context: Context {
            database: test_db.database().clone(),
            config: Config::default(),
            hostname: "web1".to_owned(),
        },
        registry,
        remarkup: phab_web::remarkup::new_engine(&Config::default()),
        php: None,
        login: None,
        shadow: Some(shadow),
    });
    let app = phab_server::router(Arc::clone(&state));

    for method in ["test.same", "test.different", "test.missing"] {
        let response = app
            .clone()
            .oneshot(
                Request::post(format!("/api/{method}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            &body[..],
            br#"{"result":42,"error_code":null,"error_info":null}"#
        );
    }

    let expected = json!({
        "test.different": {"matched": 0, "differed": 1, "skipped": 0},
        "test.missing": {"matched": 0, "differed": 0, "skipped": 1},
        "test.same": {"matched": 1, "differed": 0, "skipped": 0},
    });
    let mut counts = Value::Null;
    for _ in 0..50 {
        counts = state.shadow.as_ref().unwrap().counts();
        if counts == expected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(counts, expected);

    // Shadow calls write nothing, not even the call log.
    let logged: i64 = phab_db::sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {}",
        test_db.database().table("conduit", "conduit_methodcalllog")
    ))
    .fetch_one(test_db.database().pool())
    .await
    .unwrap();
    assert_eq!(logged, 0);

    test_db.drop_databases().await;
}
