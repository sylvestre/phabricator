//! The Files pages and file data against one test database.
//!
//! F1 is a text file anyone sees, F2 a PNG only its author (Root) sees, F3
//! a download, F4 a partial upload and F5 a deleted file. The expected
//! headers are those of `PhabricatorFileDataController` and
//! `AphrontFileResponse`.

// The checks share one database, so each page's are one long function.
#![allow(clippy::too_many_lines)]

use std::io::Read as _;
use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode};
use axum::response::Response;
use phab_core::{Config, hash};
use phab_db::Database;
use phab_db::testing::TestDatabase;
use phab_web::{FALLBACK_HEADER, WebState};
use serde_json::json;
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000;
const DIGEST_KEY_HEX: &str = "00112233445566778899aabbccddeeff";

const ALICE: &str = "phsid=alice-token";
const ROOT: &str = "phsid=root-token";

/// The bytes of F2: a PNG signature and some more.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest-of-image";

struct State {
    database: Database,
    config: Config,
    remarkup: phab_remarkup::Engine,
}

impl WebState for State {
    fn database(&self) -> &Database {
        &self.database
    }

    fn config(&self) -> &Config {
        &self.config
    }

    fn now(&self) -> i64 {
        NOW
    }

    fn remarkup(&self) -> &phab_remarkup::Engine {
        &self.remarkup
    }

    fn php(&self) -> Option<&phab_web::php::PhpConduit> {
        None
    }
}

fn session_key(token: &str) -> String {
    hash::hmac_sha256(token.as_bytes(), &hex::decode(DIGEST_KEY_HEX).unwrap()).unwrap()
}

async fn setup() -> Option<TestDatabase> {
    let test_db = TestDatabase::create(&[
        "user",
        "auth",
        "differential",
        "project",
        "policy",
        "spaces",
        "repository",
        "file",
        "harbormaster",
        "search",
    ])
    .await?;
    test_db
        .execute(&format!(
            "INSERT INTO `{{ns}}_user`.user (phid, userName, realName, dateCreated, dateModified, \
               conduitCertificate, isSystemAgent, isDisabled, isAdmin, isEmailVerified, isApproved, \
               accountSecret, isEnrolledInMultiFactor, isMailingList) VALUES \
             ('PHID-USER-alice', 'alice', 'Alice', 1, 1, 'c', 0, 0, 0, 1, 1, 's', 0, 0), \
             ('PHID-USER-root', 'root', 'Root', 1, 1, 'c', 0, 0, 1, 1, 1, 's', 0, 0); \
             INSERT INTO `{{ns}}_auth`.auth_hmackey (keyName, keyValue, dateCreated, dateModified) \
             VALUES ('session.digest', '{DIGEST_KEY_HEX}', 1, 1), ('csrf', '6b3379', 1, 1); \
             INSERT INTO `{{ns}}_user`.phabricator_session \
               (userPHID, type, sessionKey, sessionStart, sessionExpires, phid, isPartial) VALUES \
             ('PHID-USER-alice', 'web', '{alice}', 1, {expires}, 'PHID-SSSN-1', 0), \
             ('PHID-USER-root', 'web', '{root}', 1, {expires}, 'PHID-SSSN-3', 0); \
             INSERT INTO `{{ns}}_file`.file_storageblob (id, data, dateCreated, dateModified) VALUES \
             (1, 'hello world, hello files', 1, 1), (2, UNHEX('{png}'), 1, 1), \
             (3, 'PK-binary', 1, 1); \
             INSERT INTO `{{ns}}_file`.file (id, phid, name, mimeType, byteSize, storageEngine, \
               storageFormat, storageHandle, dateCreated, dateModified, authorPHID, secretKey, \
               metadata, mailKey, viewPolicy, isPartial, isDeleted, ttl, isExplicitUpload) VALUES \
             (1, 'PHID-FILE-1', 'notes \"v1\".txt', 'text/plain; charset=us-ascii', 24, 'blob', \
               'raw', '1', 100, 100, 'PHID-USER-alice', 'key1xxxxxxxxxxxxxxxx', '{{}}', 'k', \
               'users', 0, 0, NULL, 1), \
             (2, 'PHID-FILE-2', 'shot.png', 'image/png', {png_len}, 'blob', 'raw', '2', 200, 200, \
               'PHID-USER-root', 'key2xxxxxxxxxxxxxxxx', \
               '{{\"width\":640,\"height\":480,\"canCDN\":1}}', 'k', 'no-one', 0, 0, NULL, 1), \
             (3, 'PHID-FILE-3', 'a.zip', 'application/zip', 9, 'blob', 'raw', '3', 300, 300, \
               'PHID-USER-root', 'key3xxxxxxxxxxxxxxxx', '{{}}', 'k', 'users', 0, 0, {ttl}, 0), \
             (4, 'PHID-FILE-4', 'half.bin', 'application/octet-stream', 9, 'chunks', 'raw', \
               'h', 400, 400, 'PHID-USER-root', 'key4xxxxxxxxxxxxxxxx', '{{}}', 'k', 'users', 1, \
               0, NULL, 1), \
             (5, 'PHID-FILE-5', 'gone.txt', 'text/plain', 24, 'blob', 'raw', '1', 500, 500, \
               'PHID-USER-root', 'key5xxxxxxxxxxxxxxxx', '{{}}', 'k', 'users', 0, 1, NULL, 1)",
            alice = session_key("alice-token"),
            root = session_key("root-token"),
            expires = NOW + 29 * 86_400,
            png = hex::encode(PNG),
            png_len = PNG.len(),
            ttl = NOW + 3600,
        ))
        .await;
    Some(test_db)
}

fn config() -> Config {
    let mut config = Config::default();
    if let serde_json::Value::Object(map) = json!({"phabricator.base-uri": "http://phab.test/"}) {
        config.push_layer("test", map);
    }
    config
}

async fn send(test_db: &TestDatabase, request: Request<Body>) -> Response {
    let config = config();
    let state = Arc::new(State {
        database: test_db.database().clone(),
        remarkup: phab_web::remarkup::new_engine(&config),
        config,
    });
    phab_web::router::<State>()
        .with_state(state)
        .oneshot(request)
        .await
        .unwrap()
}

async fn get(test_db: &TestDatabase, path: &str, headers: &[(&str, &str)]) -> Response {
    let mut request = Request::get(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    send(test_db, request.body(Body::empty()).unwrap()).await
}

async fn bytes(response: Response) -> Vec<u8> {
    body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

fn header_value<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().unwrap())
}

async fn fallback(test_db: &TestDatabase, path: &str, headers: &[(&str, &str)]) -> String {
    let response = get(test_db, path, headers).await;
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST, "{path}");
    response.headers()[FALLBACK_HEADER]
        .to_str()
        .unwrap()
        .to_owned()
}

async fn page(test_db: &TestDatabase, path: &str, cookie: &str) -> String {
    let response = get(test_db, path, &[("cookie", cookie)]).await;
    let status = response.status();
    let reason = header_value(&response, FALLBACK_HEADER).map(str::to_owned);
    let text = String::from_utf8(bytes(response).await).unwrap();
    assert_eq!(status, StatusCode::OK, "{path}: {reason:?} {text}");
    text
}

const CSP: &str = "default-src 'self' http://phab.test; img-src 'self' http://phab.test data:; \
     style-src 'self' http://phab.test 'unsafe-inline'; script-src 'self' http://phab.test; \
     connect-src 'self'; frame-src 'self'; frame-ancestors 'none'; object-src 'none'; \
     form-action 'self'; base-uri 'none'";

async fn file_data(test_db: &TestDatabase) {
    // A text file shown in the browser: compressed, so PHP sends no
    // length (the service's server adds one; nginx frames PHP's answer
    // in chunks instead).
    let response = get(
        test_db,
        "/file/data/key1xxxxxxxxxxxxxxxx/PHID-FILE-1/notes_v1_.txt",
        &[],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let names: Vec<&str> = response
        .headers()
        .keys()
        .map(axum::http::HeaderName::as_str)
        .collect();
    assert_eq!(
        names,
        [
            "x-frame-options",
            "content-security-policy",
            "referrer-policy",
            "content-type",
            "accept-ranges",
            "cache-control",
            "expires",
            "x-content-type-options",
            "content-length",
        ]
    );
    assert_eq!(
        header_value(&response, "content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(header_value(&response, "x-frame-options"), Some("Deny"));
    assert_eq!(
        header_value(&response, "content-security-policy"),
        Some(CSP)
    );
    assert_eq!(
        header_value(&response, "cache-control"),
        Some("max-age=2592000, private")
    );
    assert_eq!(
        header_value(&response, "expires"),
        Some("Thu, 14 Dec 2023 22:13:20 GMT")
    );
    assert_eq!(bytes(response).await, b"hello world, hello files");

    // With gzip, as zlib.output_compression answers; HSTS over HTTPS.
    let response = get(
        test_db,
        "/file/data/@instance/key1xxxxxxxxxxxxxxxx/PHID-FILE-1/x/y",
        &[
            ("accept-encoding", "deflate, gzip"),
            ("x-forwarded-proto", "https"),
        ],
    )
    .await;
    assert_eq!(header_value(&response, "content-encoding"), Some("gzip"));
    assert_eq!(header_value(&response, "vary"), Some("Accept-Encoding"));
    assert_eq!(
        header_value(&response, "strict-transport-security"),
        Some("max-age=0; includeSubdomains; preload")
    );
    let mut text = String::new();
    flate2::read::GzDecoder::new(&bytes(response).await[..])
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "hello world, hello files");

    // A download without a session: the file's own type and its name.
    let response = get(
        test_db,
        "/file/download/key1xxxxxxxxxxxxxxxx/PHID-FILE-1/notes_v1_.txt",
        &[],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_value(&response, "content-type"),
        Some("text/plain; charset=us-ascii")
    );
    assert_eq!(
        header_value(&response, "x-download-options"),
        Some("noopen")
    );
    assert_eq!(
        header_value(&response, "content-disposition"),
        Some(r#"attachment; filename="notes \"v1\".txt""#)
    );
    // With a session, PHP asks first.
    assert_eq!(
        fallback(
            test_db,
            "/file/download/key1xxxxxxxxxxxxxxxx/PHID-FILE-1/notes_v1_.txt",
            &[("cookie", ALICE)],
        )
        .await,
        "the download dialog"
    );
    // ...but not for Git LFS.
    let response = get(
        test_db,
        "/file/download/key1xxxxxxxxxxxxxxxx/PHID-FILE-1/notes_v1_.txt",
        &[("cookie", ALICE), ("x-phabricator-request-type", "git-lfs")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    // A viewable image whatever its policy: the key is the check. Images
    // aren't compressed, may go to a CDN and take ranges.
    let response = get(
        test_db,
        "/file/data/key2xxxxxxxxxxxxxxxx/PHID-FILE-2/shot.png",
        &[("accept-encoding", "gzip")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, "content-type"), Some("image/png"));
    assert_eq!(
        header_value(&response, "content-length"),
        Some(PNG.len().to_string().as_str())
    );
    assert_eq!(header_value(&response, "content-encoding"), None);
    assert_eq!(
        header_value(&response, "cache-control"),
        Some("max-age=2592000, public")
    );
    assert_eq!(bytes(response).await, PNG);
    let response = get(
        test_db,
        "/file/data/key2xxxxxxxxxxxxxxxx/PHID-FILE-2/shot.png",
        &[("range", "bytes=1-3")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        header_value(&response, "content-range"),
        Some(format!("bytes 1-3/{}", PNG.len()).as_str())
    );
    assert_eq!(header_value(&response, "content-length"), Some("3"));
    assert_eq!(bytes(response).await, b"PNG");
    let response = get(
        test_db,
        "/file/data/key2xxxxxxxxxxxxxxxx/PHID-FILE-2/shot.png",
        &[("range", "bytes=20-")],
    )
    .await;
    assert_eq!(
        header_value(&response, "content-range"),
        Some(format!("bytes 20-{}/{}", PNG.len() - 1, PNG.len()).as_str())
    );
    assert_eq!(bytes(response).await, &PNG[20..]);
    assert_eq!(
        fallback(
            test_db,
            "/file/data/key2xxxxxxxxxxxxxxxx/PHID-FILE-2/shot.png",
            &[("range", "bytes=99-")],
        )
        .await,
        "an unusual Range"
    );

    // A file browsers don't show is downloaded, here without compression.
    let response = get(
        test_db,
        "/file/data/key3xxxxxxxxxxxxxxxx/PHID-FILE-3/a.zip",
        &[("accept-encoding", "gzip")],
    )
    .await;
    assert_eq!(
        header_value(&response, "content-type"),
        Some("application/zip")
    );
    assert_eq!(header_value(&response, "content-length"), Some("9"));
    assert_eq!(
        header_value(&response, "content-disposition"),
        Some("attachment; filename=\"a.zip\"")
    );

    // PHP's dialogs and 404s.
    for (path, reason) in [
        (
            "/file/data/wrongxxxxxxxxxxxxxxx/PHID-FILE-1/x",
            "a file URI with a wrong key",
        ),
        (
            "/file/data/key4xxxxxxxxxxxxxxxx/PHID-FILE-4/x",
            "a partial upload",
        ),
        (
            "/file/data/key5xxxxxxxxxxxxxxxx/PHID-FILE-5/x",
            "an unknown file",
        ),
        (
            "/file/data/key1xxxxxxxxxxxxxxxx/PHID-FILE-9/x",
            "an unknown file",
        ),
        (
            "/file/data/key1xxxxxxxxxxxxxxxx",
            "a file URI PHP doesn't route",
        ),
    ] {
        assert_eq!(fallback(test_db, path, &[]).await, reason, "{path}");
    }
    let response = send(
        test_db,
        Request::post("/file/download/key1xxxxxxxxxxxxxxxx/PHID-FILE-1/x")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
}

async fn file_pages(test_db: &TestDatabase) {
    let page_text = page(test_db, "/F1", ALICE).await;
    assert!(
        page_text.contains("notes &#34;v1&#34;.txt</h1>"),
        "{page_text}"
    );
    assert!(
        page_text.contains("<pre class=\"file-text\">hello world, hello files</pre>"),
        "{page_text}"
    );
    assert!(
        page_text.contains("<dt>Mime Type</dt><dd>text/plain; charset=us-ascii</dd>"),
        "{page_text}"
    );
    assert!(
        page_text.contains("<dt>Storage Format</dt><dd>Raw Data</dd>"),
        "{page_text}"
    );
    assert!(
        page_text.contains(
            "href=\"http://phab.test/file/data/key1xxxxxxxxxxxxxxxx/PHID-FILE-1/notes_v1_.txt\""
        ),
        "{page_text}"
    );
    // Alice wrote it, so she may edit it.
    assert!(page_text.contains("href=\"/file/edit/1/\""), "{page_text}");
    assert!(page_text.contains("<h2>Size</h2>"), "{page_text}");

    // Root's image: hidden from Alice (PHP's policy page), shown to Root.
    assert_eq!(
        fallback(test_db, "/F2", &[("cookie", ALICE)]).await,
        "a file the viewer can't see"
    );
    let page_text = page(test_db, "/F2", ROOT).await;
    assert!(
        page_text.contains("<img src=\"http://phab.test/file/data/"),
        "{page_text}"
    );
    assert!(page_text.contains("640px × 480px"), "{page_text}");
    assert!(
        page_text.contains("<dt>Attributes</dt><dd>Image, Can CDN</dd>"),
        "{page_text}"
    );
    let page_text = page(test_db, "/file/view/3/", ROOT).await;
    assert!(
        page_text.contains("<li class=\"tag\">Temporary</li>"),
        "{page_text}"
    );
    assert!(page_text.contains("Download File"), "{page_text}");

    let response = get(test_db, "/file/info/PHID-FILE-1/", &[("cookie", ALICE)]).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(header_value(&response, "location"), Some("/F1"));
    let response = get(test_db, "/F5", &[("cookie", ALICE)]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // The lists: Root's explicit uploads, then everything Alice sees.
    let list = page(test_db, "/file/", ROOT).await;
    assert!(list.contains("<h1>Authored</h1>"), "{list}");
    assert!(list.contains("/file/info/PHID-FILE-2/"), "{list}");
    assert!(!list.contains("/file/info/PHID-FILE-3/"), "{list}");
    assert!(!list.contains("/file/info/PHID-FILE-1/"), "{list}");
    let list = page(test_db, "/file/query/all/", ALICE).await;
    assert!(list.contains("/file/info/PHID-FILE-1/"), "{list}");
    assert!(list.contains("/file/info/PHID-FILE-3/"), "{list}");
    assert!(!list.contains("/file/info/PHID-FILE-2/"), "{list}");
    assert!(!list.contains("PHID-FILE-5"), "{list}");
    assert!(
        list.contains("Uploaded by <a href=\"/p/alice/\">alice</a> on 1970-01-01 00:01 UTC"),
        "{list}"
    );
    assert_eq!(
        fallback(test_db, "/file/query/abcdef/", &[("cookie", ALICE)]).await,
        "a saved file query"
    );
    assert_eq!(
        fallback(test_db, "/file/?authorPHIDs=x", &[("cookie", ALICE)]).await,
        "a search query"
    );
}

#[tokio::test]
async fn files() {
    let Some(test_db) = setup().await else {
        return;
    };
    file_data(&test_db).await;
    file_pages(&test_db).await;
    test_db.drop_databases().await;
}
