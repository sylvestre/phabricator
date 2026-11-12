use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;

async fn ping(context: &Context, body: &str) -> Response {
    let mut registry = Registry::default();
    phab_app_conduit::register(&mut registry);
    let http = HttpRequest {
        body: body.to_owned(),
        ..HttpRequest::default()
    };
    call::dispatch(context, &registry, "conduit.ping", &http, Mode::Serve, 1)
        .await
        .response
}

#[tokio::test]
async fn returns_the_host_name_without_authentication() {
    let Some(test_db) = TestDatabase::create(&["conduit"]).await else {
        return;
    };
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web-7".to_owned(),
    };

    let expected = Response::Envelope(Envelope::success("web-7".into()));
    assert_eq!(ping(&context, "").await, expected);
    // A bad token doesn't matter: the method doesn't authenticate.
    assert_eq!(ping(&context, "api.token=nonsense").await, expected);
    assert_eq!(
        ping(&context, "params=%7B%22x%22%3A1%7D").await,
        Response::Envelope(Envelope::error(
            "ERR-CONDUIT-CALL",
            "API Method \"conduit.ping\" does not define these parameters: 'x'."
        ))
    );

    test_db.drop_databases().await;
}
