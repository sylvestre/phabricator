//! The BMO `OAuth2` adapter: `PhutilBMOAuthAdapter` on top of
//! `PhutilOAuthAuthAdapter`, and the client state check of
//! `PhabricatorAuthProvider` (`getAuthCSRFCode()`).

use std::fmt::{self, Write as _};
use std::time::Duration;

use phab_core::{Config, hash, php};
use serde_json::{Map, Value};

use crate::AuthError;
use crate::provider::ClientSecret;

/// The scope BMO grants (`PhutilBMOAuthAdapter::getScope()`).
pub const SCOPE: &str = "user:read";
/// `User-Agent` of the account request (`__CLASS__`).
const USER_AGENT: &str = "PhutilBMOAuthAdapter";
/// How long a request to Bugzilla may take.
const TIMEOUT: Duration = Duration::from_secs(30);
/// `60 * 60 * 24 * 365 * 40`: smaller `expires` values are relative.
const RELATIVE_EXPIRY_LIMIT: i64 = 60 * 60 * 24 * 365 * 40;

/// An HTTP client for Bugzilla.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("a client without custom TLS roots always builds")
}

/// `getAuthCSRFCode()`: the OAuth `state`, the weak digest of the
/// `phcid` cookie.
pub fn client_state(config: &Config, client_id_cookie: &str) -> Result<String, AuthError> {
    Ok(hash::weak_digest(
        client_id_cookie.as_bytes(),
        config.hmac_key()?.as_bytes(),
    )?)
}

/// `verifyAuthCSRFCode()`: whether the provider sent back the state made
/// from this browser's `phcid` cookie, compared in constant time.
pub fn is_valid_state(
    config: &Config,
    client_id_cookie: &str,
    state: &str,
) -> Result<bool, AuthError> {
    if state.is_empty() {
        return Ok(false);
    }
    let expected = client_state(config, client_id_cookie)?;
    Ok(hash::hashes_are_identical(
        state.as_bytes(),
        expected.as_bytes(),
    ))
}

/// `rawurlencode()`.
pub fn rawurlencode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// The OAuth application registered with Bugzilla.
#[derive(Clone)]
pub struct OAuthApp {
    /// `bugzilla.url`, without a trailing slash.
    pub bugzilla_url: String,
    /// `oauth:app:id`.
    pub client_id: String,
    /// `oauth:app:secret`.
    pub client_secret: ClientSecret,
    /// The callback, `PhabricatorEnv::getURI($provider->getLoginURI())`.
    pub redirect_uri: String,
}

impl fmt::Debug for OAuthApp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthApp")
            .field("bugzilla_url", &self.bugzilla_url)
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

impl OAuthApp {
    /// `getAuthenticateURI()`: where the login button sends the browser.
    pub fn authenticate_uri(&self, scope: &str, state: &str) -> String {
        let params = [
            ("client_id", self.client_id.as_str()),
            ("scope", scope),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("state", state),
            ("response_type", "code"),
        ];
        let query: Vec<String> = params
            .iter()
            .map(|(key, value)| format!("{}={}", rawurlencode(key), rawurlencode(value)))
            .collect();
        format!("{}/oauth/authorize?{}", self.bugzilla_url, query.join("&"))
    }
}

/// An access token, never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken(String);

impl AccessToken {
    /// The token, to store on the account as PHP does.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccessToken(<redacted>)")
    }
}

/// The answer of the token endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenGrant {
    /// `access_token`.
    pub access_token: AccessToken,
    /// `expires_epoch`: when the token expires, if the provider said.
    pub expires: Option<i64>,
}

/// Why the exchange with Bugzilla failed. PHP throws in all these cases;
/// the messages are for the log, never shown with secrets.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// Bugzilla could not be reached.
    #[error("the request to Bugzilla failed: {0}")]
    Transport(#[from] reqwest::Error),
    /// Bugzilla answered with an HTTP error.
    #[error("Bugzilla answered with HTTP {0}")]
    Status(u16),
    /// The token answer had neither a token nor an error.
    #[error("failed to decode the OAuth access token response")]
    BadTokenResponse,
    /// The token endpoint returned an error.
    #[error("access token error: {0}")]
    TokenError(String),
    /// The account answer was not a JSON object.
    #[error("expected valid JSON response from BMO account data request")]
    BadAccountResponse,
}

/// A number the way PHP's `<` and `+=` read it from a JSON value;
/// fractions are dropped.
#[allow(clippy::cast_possible_truncation)]
fn php_number(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|float| float as i64)),
        Value::String(text) => text.trim().parse::<f64>().ok().map(|float| float as i64),
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

/// `readAccessTokenResponse()` and the expiry handling of
/// `makeTokenRequest()`.
fn read_token_response(body: &str, now: i64) -> Result<TokenGrant, OAuthError> {
    let data: Map<String, Value> = match serde_json::from_str::<Value>(body) {
        Ok(Value::Object(data)) => data,
        // PHP falls back to `parse_str()`.
        _ => form_urlencoded::parse(body.as_bytes())
            .map(|(key, value)| (key.into_owned(), Value::String(value.into_owned())))
            .collect(),
    };
    let truthy = |key: &str| data.get(key).is_some_and(php::is_truthy);
    if !truthy("access_token") && !truthy("error") {
        return Err(OAuthError::BadTokenResponse);
    }
    let expires = data
        .get("expires_in")
        .filter(|value| !value.is_null())
        .or_else(|| data.get("expires").filter(|value| !value.is_null()))
        .and_then(php_number)
        .map(|expires| {
            if expires < RELATIVE_EXPIRY_LIMIT {
                expires + now
            } else {
                expires
            }
        });
    if let Some(error) = data.get("error").filter(|value| !value.is_null()) {
        return Err(OAuthError::TokenError(php::string_cast(error).into_owned()));
    }
    let token = data
        .get("access_token")
        .map(php::string_cast)
        .unwrap_or_default();
    Ok(TokenGrant {
        access_token: AccessToken(token.into_owned()),
        expires,
    })
}

/// `loadAccessTokenData()`: trades the authorization `code` for a token.
pub async fn exchange_code(
    client: &reqwest::Client,
    app: &OAuthApp,
    code: &str,
    now: i64,
) -> Result<TokenGrant, OAuthError> {
    let form = [
        ("client_id", app.client_id.as_str()),
        ("client_secret", app.client_secret.expose()),
        ("redirect_uri", app.redirect_uri.as_str()),
        ("code", code),
        ("grant_type", "authorization_code"),
    ];
    let response = client
        .post(format!("{}/oauth/access_token", app.bugzilla_url))
        .form(&form)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(OAuthError::Status(status.as_u16()));
    }
    let body = response.text().await?;
    read_token_response(&body, now)
}

/// The BMO account (`/api/user/profile`), as the adapter reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct BmoAccount {
    data: Map<String, Value>,
}

impl BmoAccount {
    fn text(&self, key: &str) -> Option<String> {
        match self.data.get(key) {
            None | Some(Value::Null) => None,
            Some(value) => Some(php::string_cast(value).into_owned()),
        }
    }

    /// `getAccountID()`: the BMO user id.
    pub fn id(&self) -> Option<String> {
        self.text("id")
    }

    /// `getAccountEmail()`.
    pub fn email(&self) -> Option<String> {
        self.text("login")
    }

    /// `getAccountName()`.
    pub fn username(&self) -> Option<String> {
        self.text("nick")
    }

    /// `getAccountRealName()`.
    pub fn real_name(&self) -> Option<String> {
        self.text("name")
    }

    /// `getAccountMFA()`, read with PHP truthiness.
    pub fn has_mfa(&self) -> bool {
        self.data.get("mfa").is_some_and(php::is_truthy)
    }
}

/// `loadOAuthAccountData()`.
pub async fn load_account(
    client: &reqwest::Client,
    bugzilla_url: &str,
    token: &AccessToken,
) -> Result<BmoAccount, OAuthError> {
    let response = client
        .get(format!("{bugzilla_url}/api/user/profile"))
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", token.0),
        )
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(OAuthError::Status(status.as_u16()));
    }
    let body = response.text().await?;
    match serde_json::from_str::<Value>(&body) {
        Ok(Value::Object(data)) => Ok(BmoAccount { data }),
        // `phutil_json_decode()` refuses anything but an object or a list;
        // a list has none of the account keys.
        Ok(Value::Array(_)) => Ok(BmoAccount { data: Map::new() }),
        _ => Err(OAuthError::BadAccountResponse),
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    const NOW: i64 = 1_700_000_000;

    fn app() -> OAuthApp {
        OAuthApp {
            bugzilla_url: "https://bmo.example".to_owned(),
            client_id: "client id".to_owned(),
            client_secret: ClientSecret::new("s3cret"),
            redirect_uri: "https://phab.example/auth/login/bmo:bugzilla.mozilla.org/".to_owned(),
        }
    }

    #[test]
    fn authenticate_uris_are_built_like_php() {
        assert_eq!(
            app().authenticate_uri(SCOPE, "abc"),
            "https://bmo.example/oauth/authorize?client_id=client%20id&scope=user%3Aread&\
             redirect_uri=https%3A%2F%2Fphab.example%2Fauth%2Flogin%2Fbmo%3Abugzilla.mozilla.org%2F&\
             state=abc&response_type=code"
        );
    }

    #[test]
    fn states_compare_against_the_cookie() {
        let config = Config::default();
        let state = client_state(&config, "cookie").unwrap();
        // PHP: hash_hmac('sha1', 'cookie', <default security.hmac-key>).
        assert_eq!(state.len(), 40);
        assert!(is_valid_state(&config, "cookie", &state).unwrap());
        assert!(!is_valid_state(&config, "other", &state).unwrap());
        assert!(!is_valid_state(&config, "cookie", "").unwrap());
        assert!(!is_valid_state(&config, "cookie", &state[..39]).unwrap());
    }

    #[rstest]
    #[case(r#"{"access_token":"t","expires_in":3600}"#, Some(NOW + 3600))]
    #[case(r#"{"access_token":"t","expires":"3600"}"#, Some(NOW + 3600))]
    #[case(r#"{"access_token":"t","expires_in":2000000000}"#, Some(2_000_000_000))]
    #[case(r#"{"access_token":"t"}"#, None)]
    #[case("access_token=t&expires_in=60", Some(NOW + 60))]
    fn token_responses(#[case] body: &str, #[case] expires: Option<i64>) {
        let grant = read_token_response(body, NOW).unwrap();
        assert_eq!(grant.access_token.expose(), "t");
        assert_eq!(grant.expires, expires);
    }

    #[rstest]
    #[case(r#"{"error":"invalid_grant"}"#)]
    #[case(r#"{"access_token":"t","error":"x"}"#)]
    #[case(r#"{"access_token":""}"#)]
    #[case("garbage")]
    fn bad_token_responses(#[case] body: &str) {
        assert!(read_token_response(body, NOW).is_err());
    }

    #[test]
    fn accounts_read_like_php() {
        let account = BmoAccount {
            data: json!({"id": 42, "login": "a@example.com", "nick": "alice", "name": null, "mfa": "0"})
                .as_object()
                .unwrap()
                .clone(),
        };
        assert_eq!(account.id().as_deref(), Some("42"));
        assert_eq!(account.email().as_deref(), Some("a@example.com"));
        assert_eq!(account.username().as_deref(), Some("alice"));
        assert_eq!(account.real_name(), None);
        assert!(!account.has_mfa());
    }

    #[test]
    fn tokens_are_not_printed() {
        let grant = read_token_response(r#"{"access_token":"t0ps3cret"}"#, NOW).unwrap();
        assert!(!format!("{grant:?}").contains("t0ps3cret"));
        assert!(!format!("{:?}", app()).contains("s3cret"));
    }
}
