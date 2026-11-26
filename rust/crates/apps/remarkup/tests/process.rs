//! `remarkup.process` against a test database.

use phab_conduit::{Context, Envelope, HttpRequest, Mode, Registry, Response, call};
use phab_core::Config;
use phab_db::testing::TestDatabase;
use serde_json::{Value, json};

const TOKEN: &str = "api-rmrmrmrmrmrmrmrmrmrmrmrmrmrm";

async fn query(context: &Context, params: Value) -> Response {
    let mut registry = Registry::default();
    phab_app_remarkup::register(&mut registry);
    let mut params = params;
    params["__conduit__"] = json!({"token": TOKEN});
    let body: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("params", &params.to_string())
        .finish();
    let http = HttpRequest {
        body,
        ..HttpRequest::default()
    };
    call::dispatch(
        context,
        &registry,
        "remarkup.process",
        &http,
        Mode::Shadow,
        1_700_000_000,
    )
    .await
    .response
}

async fn envelope(context: &Context, params: Value) -> Envelope {
    match query(context, params.clone()).await {
        Response::Envelope(envelope) => envelope,
        other @ Response::Fallback(_) => panic!("{params}: expected an answer, got {other:?}"),
    }
}

async fn answer(context: &Context, params: Value) -> Value {
    let envelope = envelope(context, params.clone()).await;
    assert_eq!(envelope.error_code, None, "{params}: {envelope:?}");
    envelope.result
}

async fn setup(settings: Value) -> Option<(TestDatabase, Context)> {
    let test_db = TestDatabase::create(&["user", "auth", "conduit"]).await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-bob', 'bob', 'Bob', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_conduit`.conduit_token \
               (objectPHID, tokenType, token, expires, dateCreated, dateModified) VALUES \
             ('PHID-USER-bob', 'api', '{TOKEN}', NULL, 1, 1)"
        ))
        .await;
    let mut config = Config::default();
    let mut layer = json!({
        "phabricator.base-uri": "http://phabricator.test/",
        "uri.allowed-protocols": {"http": true, "https": true},
        "bugzilla.url": "https://bugzilla.test/home",
    });
    if let (Value::Object(layer), Value::Object(settings)) = (&mut layer, settings) {
        layer.extend(settings);
    }
    if let Value::Object(map) = layer {
        config.push_layer("test", map);
    }
    let context = Context {
        database: test_db.database().clone(),
        config,
        hostname: "web1".to_owned(),
    };
    Some((test_db, context))
}

#[tokio::test]
async fn renders_libphutil_remarkup() {
    let Some((test_db, context)) = setup(json!({})).await else {
        return;
    };

    assert_eq!(
        answer(
            &context,
            json!({
                "context": "differential",
                "contents": ["**bold** and //it//", "a\nb", "= Title =", ""],
            })
        )
        .await,
        json!([
            {"content": "<p><strong>bold</strong> and <em>it</em></p>"},
            {"content": "<p>a<br />\nb</p>"},
            {"content": "<h2 class=\"remarkup-header\">Title</h2>"},
            {"content": ""},
        ])
    );
    // Diffusion's headers get anchors; a dictionary is read as a list.
    assert_eq!(
        answer(
            &context,
            json!({"context": "diffusion", "contents": {"x": "= Title =", "y": "`code`"}})
        )
        .await,
        json!([
            {"content": "<h2 class=\"remarkup-header\"><a name=\"title\"></a>Title</h2>"},
            {"content": "<p><tt class=\"remarkup-monospaced\">code</tt></p>"},
        ])
    );
    assert_eq!(
        answer(
            &context,
            json!({"context": "feed", "contents": ["see http://example.com/x"]})
        )
        .await,
        json!([{"content": "<p>see <a href=\"http://example.com/x\" class=\"remarkup-link\" \
                            target=\"_blank\" rel=\"noreferrer\">http://example.com/x</a></p>"}])
    );
    // Links to bugs, from moz-extensions.
    assert_eq!(
        answer(
            &context,
            json!({"context": "differential", "contents": ["Bug 123 comment 4, bug 5"]})
        )
        .await,
        json!([{"content": "<p><a href=\"https://bugzilla.test/show_bug.cgi?id=123#c4\">Bug 123 \
                            comment 4</a>, <a href=\"https://bugzilla.test/show_bug.cgi?id=5\">bug \
                            5</a></p>"}])
    );
    // Code without a language is plain text.
    assert_eq!(
        answer(
            &context,
            json!({"context": "feed", "contents": ["  x < y"]})
        )
        .await,
        json!([{"content": "<div class=\"remarkup-code-block\" data-code-lang=\"text\" \
                            data-sigil=\"remarkup-code-block\"><pre class=\"remarkup-code\">x \
                            &lt; y</pre></div>"}])
    );
    assert_eq!(
        answer(&context, json!({"context": "feed", "contents": []})).await,
        json!([])
    );

    test_db.drop_databases().await;
}

#[tokio::test]
async fn rejects_unknown_contexts() {
    let Some((test_db, context)) = setup(json!({})).await else {
        return;
    };

    for params in [
        json!({"context": "nope", "contents": ["x"]}),
        json!({"contents": ["x"]}),
        // The context is checked first.
        json!({"context": 3, "contents": 5}),
    ] {
        let envelope = envelope(&context, params.clone()).await;
        assert_eq!(
            (
                envelope.error_code.as_deref(),
                envelope.error_info.as_deref()
            ),
            (Some("ERR-INVALID_ENGINE"), Some("Unknown Error")),
            "{params}"
        );
    }

    test_db.drop_databases().await;
}

/// Without Pygments, PHP shows code in every language as plain text since
/// Bug 2093445 (its PHP, Python, Java and JSON lexers and its console
/// highlighter are gone); Pygments is on by default.
#[tokio::test]
async fn shows_code_as_plain_text_without_pygments() {
    let Some((test_db, context)) = setup(json!({"pygments.enabled": false})).await else {
        return;
    };
    for (language, code, html) in [
        (
            "php",
            "<?php\n$foo = 'bar';",
            "&lt;?php\n$foo = &#039;bar&#039;;",
        ),
        ("json", "{\"a\": 1}", "{&quot;a&quot;: 1}"),
        ("py", "x = 1", "x = 1"),
        ("console", "$ ls # all", "$ ls # all"),
    ] {
        assert_eq!(
            answer(
                &context,
                json!({"context": "differential", "contents": [format!("```lang={language}\n{code}\n```")]})
            )
            .await,
            json!([{"content": format!(
                "<div class=\"remarkup-code-block\" data-code-lang=\"{language}\" \
                 data-sigil=\"remarkup-code-block\"><pre class=\"remarkup-code\">{html}</pre></div>"
            )}]),
            "{language}"
        );
    }
    test_db.drop_databases().await;
    let Some((test_db, context)) = setup(json!({})).await else {
        return;
    };
    let params = json!({"context": "differential", "contents": ["```lang=php\n$x\n```"]});
    assert!(
        matches!(query(&context, params.clone()).await, Response::Fallback(_)),
        "{params}"
    );
    test_db.drop_databases().await;
}

#[tokio::test]
async fn leaves_phabricator_rules_to_php() {
    let Some((test_db, context)) = setup(json!({"pygments.enabled": true})).await else {
        return;
    };

    for content in [
        // Mentions and icons are styled for the Rust interface.
        "@bob",
        "@nobody",
        "{icon camera}",
        "{img https://example.com/a.png}",
        // Rules which are not ported, and handles the pages don't load.
        "B12",
        "{PHID-CMIT-abcdefghijklmnopqrst}",
        // Highlighted by Pygments, or PHP's own highlighters.
        "```lang=rust\nfn main() {}\n```",
        "```lang=json\n{}\n```",
        "```\n$ ls\n```",
        "```lang=rainbow\nx\n```",
    ] {
        let params = json!({"context": "differential", "contents": ["ok", content]});
        assert!(
            matches!(query(&context, params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }
    for params in [
        json!({"context": "differential", "contents": "x"}),
        json!({"context": "differential", "contents": [1]}),
        json!({"context": ["feed"], "contents": ["x"]}),
    ] {
        assert!(
            matches!(query(&context, params.clone()).await, Response::Fallback(_)),
            "{params}"
        );
    }
    // Not a PHID type of this install: text, in PHP too.
    assert_eq!(
        answer(
            &context,
            json!({"context": "differential", "contents": ["{PHID-TASK-abcdefghijklmnopqrst}"]})
        )
        .await,
        json!([{"content": "<p>{PHID-TASK-abcdefghijklmnopqrst}</p>"}])
    );
    // Plain text is never highlighted.
    assert_eq!(
        answer(
            &context,
            json!({"context": "differential", "contents": ["```lang=text\nx\n```"]})
        )
        .await,
        json!([{"content": "<div class=\"remarkup-code-block\" data-code-lang=\"text\" \
                            data-sigil=\"remarkup-code-block\"><pre class=\"remarkup-code\">x\
                            </pre></div>"}])
    );

    test_db.drop_databases().await;
}
