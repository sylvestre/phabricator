use phab_conduit::{
    ApiRequest, Application, Context, Envelope, HttpRequest, Method, MethodError, MethodFuture,
    Mode, Registry, Response, call::dispatch,
};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const NOW: i64 = 1_700_000_000;
const TOKEN: &str = "api-aaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Echo;

impl Method for Echo {
    fn name(&self) -> &'static str {
        "test.echo"
    }

    fn params(&self) -> &'static [&'static str] {
        &["value", "fail"]
    }

    fn application(&self) -> Option<Application> {
        Some(Application {
            class: "PhabricatorDifferentialApplication",
            name: "Differential",
        })
    }

    fn error_types(&self) -> &'static [(&'static str, &'static str)] {
        &[("ERR-KNOWN", "A known error.")]
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            match request.value("fail").and_then(Value::as_str) {
                Some("known") => Err(MethodError::Conduit {
                    code: "ERR-KNOWN".to_owned(),
                    info: None,
                }),
                Some("unsupported") => Err(MethodError::Unsupported("not ported")),
                _ => Ok(json!({
                    "value": request.value("value"),
                    "user": request.user.as_ref().map(|user| user.user_name.clone()),
                    "typed": request.strictly_typed,
                })),
            }
        })
    }
}

fn config(values: Value) -> Config {
    let mut config = Config::default();
    if let Value::Object(map) = values {
        config.push_layer("test", map);
    }
    config
}

async fn setup() -> Option<TestDatabase> {
    let test_db =
        TestDatabase::create(&["user", "auth", "conduit", "project", "policy", "spaces"]).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-alice', 'api', '{TOKEN}', NULL, 1, 1)"
        ))
        .await;
    Some(test_db)
}

async fn call(context: &Context, body: &str, mode: Mode) -> Response {
    let mut registry = Registry::default();
    registry.register(Echo);
    let http = HttpRequest {
        content_type: Some("application/x-www-form-urlencoded".to_owned()),
        query: "__path__=/api/test.echo".to_owned(),
        body: body.to_owned(),
        session_cookie: None,
    };
    dispatch(context, &registry, "test.echo", &http, mode, NOW)
        .await
        .response
}

fn envelope(response: Response) -> Envelope {
    match response {
        Response::Envelope(envelope) => envelope,
        Response::Fallback(reason) => panic!("unexpected fallback: {reason}"),
    }
}

#[tokio::test]
async fn dispatches_like_php() {
    let Some(test_db) = setup().await else {
        return;
    };
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };

    let ok = envelope(
        call(
            &context,
            &format!("api.token={TOKEN}&value=hi"),
            Mode::Serve,
        )
        .await,
    );
    assert_eq!(
        ok,
        Envelope::success(json!({"value": "hi", "user": "alice", "typed": false}))
    );
    assert_eq!(
        ok.to_body(),
        r#"{"result":{"value":"hi","user":"alice","typed":false},"error_code":null,"error_info":null}"#
    );

    let typed = envelope(
        call(
            &context,
            &format!("params=%7B%22value%22%3A3%2C%22__conduit__%22%3A%7B%22token%22%3A%22{TOKEN}%22%7D%7D"),
            Mode::Serve,
        )
        .await,
    );
    assert_eq!(
        typed.result,
        json!({"value": 3, "user": "alice", "typed": true})
    );

    assert_eq!(
        envelope(call(&context, "value=1&other=2&more=3", Mode::Serve).await),
        Envelope::error(
            "ERR-CONDUIT-CALL",
            "API Method \"test.echo\" does not define these parameters: 'other', 'more'."
        )
    );
    assert_eq!(
        envelope(call(&context, "value=1", Mode::Serve).await),
        Envelope::error("ERR-INVALID-SESSION", "Session key is not present.")
    );
    assert_eq!(
        envelope(call(&context, "api.token=api-short", Mode::Serve).await),
        Envelope::error(
            "ERR-INVALID-AUTH",
            "API token \"api-short\" has the wrong length. API tokens should be 32 characters long."
        )
    );
    assert_eq!(
        envelope(call(&context, "api.auth.type=x", Mode::Serve).await),
        Envelope::error(
            "ERR-INVALID-AUTH",
            "Provided \"auth.type\" (\"x\") is not recognized."
        )
    );
    assert_eq!(
        envelope(
            call(
                &context,
                &format!("api.token={TOKEN}&fail=known"),
                Mode::Serve
            )
            .await
        ),
        Envelope::error("ERR-KNOWN", "A known error.")
    );
    let logged: i64 = phab_db::sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {}",
        context.database.table("conduit", "conduit_methodcalllog")
    ))
    .fetch_one(context.database.pool())
    .await
    .unwrap();
    // One row per envelope above; fallbacks are logged by PHP.
    assert_eq!(logged, 7);

    call(&context, &format!("api.token={TOKEN}"), Mode::Shadow).await;
    let after_shadow: i64 = phab_db::sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {}",
        context.database.table("conduit", "conduit_methodcalllog")
    ))
    .fetch_one(context.database.pool())
    .await
    .unwrap();
    assert_eq!(after_shadow, 7);

    test_db.drop_databases().await;
}

#[tokio::test]
async fn leaves_unported_paths_to_php() {
    let Some(test_db) = setup().await else {
        return;
    };
    let context = Context {
        database: test_db.database().clone(),
        config: Config::default(),
        hostname: "web1".to_owned(),
    };
    assert_eq!(
        call(
            &context,
            &format!("api.token={TOKEN}&fail=unsupported"),
            Mode::Serve
        )
        .await,
        Response::Fallback("not ported")
    );
    assert!(matches!(
        call(&context, "api.auth.type=asymmetric", Mode::Serve).await,
        Response::Fallback(_)
    ));
    assert!(matches!(
        call(&context, "api.sessionKey=abc", Mode::Serve).await,
        Response::Fallback(_)
    ));
    assert!(matches!(
        call(&context, "output=human", Mode::Serve).await,
        Response::Fallback(_)
    ));

    test_db.drop_databases().await;
}

#[tokio::test]
async fn applies_application_policies() {
    let Some(test_db) = setup().await else {
        return;
    };
    let uninstalled = Context {
        database: test_db.database().clone(),
        config: config(json!({
            "phabricator.uninstalled-applications": {"PhabricatorDifferentialApplication": true}
        })),
        hostname: "web1".to_owned(),
    };
    assert_eq!(
        envelope(call(&uninstalled, &format!("api.token={TOKEN}"), Mode::Serve).await),
        Envelope::error(
            "ERR-CONDUIT-CALL",
            "Method 'test.echo' belongs to application 'Differential', which is not installed."
        )
    );

    let restricted = Context {
        database: test_db.database().clone(),
        config: config(json!({
            "phabricator.application-settings": {
                "PHID-APPS-PhabricatorDifferentialApplication": {"policy": {"view": "admin"}}
            }
        })),
        hostname: "web1".to_owned(),
    };
    assert_eq!(
        envelope(call(&restricted, &format!("api.token={TOKEN}"), Mode::Serve).await),
        Envelope::error(
            "You do not have access to the application which provides this API method.",
            "Unknown Error"
        )
    );

    test_db.drop_databases().await;
}
