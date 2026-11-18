//! `conduit.getcapabilities` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::json;

#[tokio::test]
async fn lists_the_capabilities_without_authentication() {
    let Some(test_db) = TestDatabase::create(&["conduit"]).await else {
        return;
    };
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web-7".to_owned(),
    };
    let mut registry = Registry::default();
    phab_app_conduit::register(&mut registry);
    let http = HttpRequest {
        body: "api.token=nonsense".to_owned(),
        ..HttpRequest::default()
    };

    assert_eq!(
        call::dispatch(
            &context,
            &registry,
            "conduit.getcapabilities",
            &http,
            Mode::Serve,
            1
        )
        .await
        .response,
        Response::Envelope(Envelope::success(json!({
            "authentication": ["token", "asymmetric", "session", "sessionless"],
            "signatures": ["consign"],
            "input": ["json", "urlencoded"],
            "output": ["json", "human"],
        })))
    );

    test_db.drop_databases().await;
}
