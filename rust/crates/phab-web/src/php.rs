//! Calls to PHP's Conduit API as the logged-in user (`RUST_PORT_PLAN.md`
//! §9.1): until the Rust transaction editor exists (W9), the pages send
//! their writes to PHP with the user's session cookie and a CSRF token,
//! exactly as a form of the old interface would.

use std::fmt;
use std::time::Duration;

use serde_json::Value;

/// The header asking nginx for PHP (`phab_parity::SKIP_RUST_HEADER`).
const SKIP_RUST_HEADER: &str = "x-phab-skip-rust";

/// How long a call to PHP may take: below nginx's 60 second
/// `proxy_read_timeout`, so the page can still say what happened.
const TIMEOUT: Duration = Duration::from_secs(45);

/// An HTTP client for PHP, which gives up on a server that doesn't accept
/// the connection or doesn't answer.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(TIMEOUT)
        .build()
        .expect("a client without custom TLS roots always builds")
}

/// A session token, never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionToken(pub(crate) String);

impl fmt::Debug for SessionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionToken(<redacted>)")
    }
}

/// Why a call failed.
#[derive(Debug, thiserror::Error)]
pub enum ConduitError {
    /// PHP could not be reached, or its answer could not be read.
    #[error("the request to PHP failed: {0}")]
    Transport(#[from] reqwest::Error),
    /// PHP answered with something other than a Conduit envelope.
    #[error("PHP answered with HTTP {0}")]
    Unexpected(u16),
    /// The method failed; PHP's message is meant for the user.
    #[error("{code}: {info}")]
    Method {
        /// `error_code`.
        code: String,
        /// `error_info`.
        info: String,
    },
}

/// PHP's Conduit API, behind nginx.
#[derive(Debug, Clone)]
pub struct PhpConduit {
    base_url: String,
    client: reqwest::Client,
    /// For forms, whose redirects are PHP's answer.
    forms: reqwest::Client,
}

/// PHP's answer to a form.
#[derive(Debug)]
pub struct FormAnswer {
    /// The HTTP status.
    pub status: u16,
    /// The `Content-Type`, if any.
    pub content_type: Option<String>,
    /// The page, for answers other than redirects.
    pub body: Vec<u8>,
}

impl PhpConduit {
    /// A client for PHP at `base_url`, like `http://phabricator.test`.
    pub fn new(base_url: impl Into<String>) -> Self {
        let forms = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("a client without custom TLS roots always builds");
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client: client(),
            forms,
        }
    }

    /// Posts `fields` to PHP's page at `path` as the user of `session`, as
    /// a form of PHP's interface would.
    pub async fn post_form_as_user(
        &self,
        path: &str,
        fields: &[(&str, String)],
        session: &SessionToken,
    ) -> Result<FormAnswer, ConduitError> {
        let response = self
            .forms
            .post(format!("{}{path}", self.base_url))
            .header(SKIP_RUST_HEADER, "1")
            .header(reqwest::header::COOKIE, format!("phsid={}", session.0))
            .form(fields)
            .send()
            .await?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.bytes().await?.to_vec();
        Ok(FormAnswer {
            status,
            content_type,
            body,
        })
    }

    /// From `PHAB_PHP_URL`, if set.
    pub fn from_env() -> Option<Self> {
        std::env::var("PHAB_PHP_URL")
            .ok()
            .filter(|url| !url.is_empty())
            .map(Self::new)
    }

    /// Calls `method` as the user of `session`, with a CSRF token made for
    /// them, and returns the `result`.
    pub async fn call_as_user(
        &self,
        method: &str,
        params: &Value,
        session: &SessionToken,
        csrf_token: &str,
    ) -> Result<Value, ConduitError> {
        let form = [
            ("params", params.to_string()),
            ("output", "json".to_owned()),
            ("__conduit__", "1".to_owned()),
            ("__csrf__", csrf_token.to_owned()),
        ];
        let response = self
            .client
            .post(format!("{}/api/{method}", self.base_url))
            .header(SKIP_RUST_HEADER, "1")
            .header(reqwest::header::COOKIE, format!("phsid={}", session.0))
            .form(&form)
            .send()
            .await?;
        let status = response.status().as_u16();
        let body = response.bytes().await?;
        let Ok(envelope) = serde_json::from_slice::<Value>(&body) else {
            return Err(ConduitError::Unexpected(status));
        };
        match envelope.get("error_code") {
            Some(Value::String(code)) => Err(ConduitError::Method {
                code: code.clone(),
                info: envelope
                    .get("error_info")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }),
            _ => Ok(envelope.get("result").cloned().unwrap_or(Value::Null)),
        }
    }
}
