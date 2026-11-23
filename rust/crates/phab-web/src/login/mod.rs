//! Web login (W7), off unless `PHAB_RUST_LOGIN=1`: the login page, the BMO
//! OAuth callback, password login for administrators, the end of the login
//! (`/auth/validate/`, `/auth/finish/` with its TOTP prompt), registration
//! of BMO accounts (`/auth/register/<key>/`), linking and refreshing BMO
//! accounts (`/auth/link/`, `/auth/refresh/`, `/auth/confirmlink/`) and
//! logout.
//!
//! Sessions, cookies and log rows are written exactly as PHP writes them, so
//! a user logged in by one stack is logged in on both. Every step which
//! Rust doesn't port hands the request to PHP with a `421` *before*
//! anything is written or sent to Bugzilla, so PHP's replay sees the same
//! state: invites, factors other than TOTP, registrations which mail a
//! verification link, rate limits, CAPTCHAs, password hashes to upgrade,
//! non-administrator passwords and every error PHP raises before the OAuth
//! code is spent. Once the code is spent it can't be replayed, so later
//! failures are answered here.

mod finish;
mod link;
mod logout;
mod page;
mod provider;
mod register;
pub(crate) mod request;
mod start;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use phab_auth::csrf::{self, CsrfKey, CsrfSecret};
use phab_auth::session::{self, SESSION_COOKIE, SessionDigest, SessionType, SessionUser};
use phab_auth::{Outcome, Writes};
use phab_core::{Config, php};

pub use finish::{finish, validate};
pub use link::{confirm as confirm_link, link, refresh};
pub use logout::logout;
pub use provider::login as provider_login;
pub use register::register;
pub use start::{logged_out, start};

use crate::error::PageError;
use crate::state::WebState;
use request::LoginRequest;

/// `PhabricatorCookies::COOKIE_USERNAME`.
const USERNAME_COOKIE: &str = "phusr";
/// `PhabricatorCookies::COOKIE_REGISTRATION`.
const REGISTRATION_COOKIE: &str = "phreg";
/// `PhabricatorCookies::COOKIE_CLIENTID`.
const CLIENT_ID_COOKIE: &str = "phcid";
/// `PhabricatorCookies::COOKIE_NEXTURI`.
const NEXT_URI_COOKIE: &str = "next_uri";
/// `PhabricatorCookies::COOKIE_HISEC`.
const HISEC_COOKIE: &str = "jump_to_hisec";
/// `PhabricatorCookies::COOKIE_INVITE`.
const INVITE_COOKIE: &str = "invite";

/// What the login pages need besides the shared state. Present only when
/// `PHAB_RUST_LOGIN=1`; without it every login route answers `421`.
#[derive(Debug, Clone)]
pub struct LoginSettings {
    client: reqwest::Client,
    hostname: String,
}

impl LoginSettings {
    /// Settings with an HTTP client for Bugzilla and the host name the
    /// activity log records.
    pub fn new(client: reqwest::Client, hostname: impl Into<String>) -> Self {
        Self {
            client,
            hostname: hostname.into(),
        }
    }

    /// From `PHAB_RUST_LOGIN`: on only when it is `1`.
    pub fn from_env(hostname: impl Into<String>) -> Option<Self> {
        (std::env::var("PHAB_RUST_LOGIN").as_deref() == Ok("1"))
            .then(|| Self::new(phab_auth::oauth::client(), hostname))
    }
}

/// The visitor of a login page, as `PhabricatorController` sets it up.
enum Visitor {
    /// A logged-in user, whose session may be partial.
    LoggedIn { found: Box<SessionUser> },
    /// A logged-out visitor with a `phsid` cookie (anonymous or stale).
    LoggedOut { token: String },
    /// No `phsid` cookie yet: PHP issues an anonymous one.
    New,
}

/// A login page request with what every one of them checks first.
struct Context<'a, S> {
    state: &'a S,
    settings: &'a LoginSettings,
    request: LoginRequest,
    visitor: Visitor,
}

impl<'a, S: WebState> Context<'a, S> {
    /// The checks common to the login pages, all without writing: Rust
    /// login is on, the install is writable, PHP could set cookies on
    /// this host, and the visitor's session is loaded.
    async fn new(
        state: &'a S,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: &Bytes,
    ) -> Result<Self, PageError> {
        let settings = state
            .login()
            .ok_or(PageError::Fallback("Rust login is off"))?;
        let config = state.config();
        if config.is_read_only() {
            return Err(PageError::Fallback("the install is read-only"));
        }
        if config.base_uri().is_err() {
            return Err(PageError::Fallback("phabricator.base-uri is not set"));
        }
        let request = LoginRequest::new(method, uri, headers, body);
        if request.cookie_domain(config).is_none() {
            return Err(PageError::Fallback("cookies can't be set on this host"));
        }
        let visitor = match request.nonempty_cookie(SESSION_COOKIE) {
            None => Visitor::New,
            Some(token) => {
                let token = token.to_owned();
                match session::load_user_for_session(
                    state.database(),
                    config,
                    SessionType::Web,
                    &token,
                    state.now(),
                    Writes::Allowed,
                )
                .await?
                {
                    Outcome::Done(Some(found)) => Visitor::LoggedIn {
                        found: Box::new(found),
                    },
                    Outcome::Done(None) => Visitor::LoggedOut { token },
                    Outcome::Unsupported(reason) => return Err(PageError::Fallback(reason)),
                }
            }
        };
        Ok(Self {
            state,
            settings,
            request,
            visitor,
        })
    }

    fn config(&self) -> &'a Config {
        self.state.config()
    }

    /// The anonymous session cookie `PhabricatorController` sets when the
    /// request has none.
    fn issue_anonymous_session(&mut self) {
        if matches!(self.visitor, Visitor::New) {
            let token = session::anonymous_token();
            let now = self.state.now();
            self.request
                .set_cookie(self.config(), SESSION_COOKIE, &token, now);
            self.visitor = Visitor::LoggedOut { token };
        }
    }

    /// What the visitor's CSRF tokens are made from, and the key.
    async fn csrf(&self) -> Result<Option<(CsrfSecret, CsrfKey)>, PageError> {
        let secret = match &self.visitor {
            Visitor::LoggedIn { found } => found.csrf_secret.clone(),
            Visitor::LoggedOut { token } => {
                match CsrfSecret::for_logged_out(self.state.database(), token).await? {
                    Outcome::Done(secret) => secret,
                    Outcome::Unsupported(reason) => return Err(PageError::Fallback(reason)),
                }
            }
            Visitor::New => return Ok(None),
        };
        let key = match csrf::load_key(self.state.database()).await? {
            Outcome::Done(key) => key,
            Outcome::Unsupported(reason) => return Err(PageError::Fallback(reason)),
        };
        Ok(Some((secret, key)))
    }

    /// A CSRF token for a form of the page.
    async fn csrf_token(&self) -> Result<String, PageError> {
        let (secret, key) = self
            .csrf()
            .await?
            .ok_or(PageError::Fallback("no session to make a CSRF token for"))?;
        Ok(csrf::new_token(&secret, &key, self.state.now())?)
    }

    /// `isFormPost()`: a POST with `__form__` and a valid CSRF token. PHP
    /// throws on a bad token; that page is PHP's.
    async fn is_form_post(&self) -> Result<bool, PageError> {
        if !self.request.is_post() || !self.request.exists("__form__") {
            return Ok(false);
        }
        if self.request.exists("__hisec__") {
            return Err(PageError::Fallback("a multi-factor form"));
        }
        self.validate_csrf().await?;
        Ok(true)
    }

    /// `validateCSRF()`: the token of the `__csrf__` field, or of the
    /// `X-Phabricator-Csrf` header when the field is `empty()`. PHP throws
    /// on a bad token, an error page which is PHP's to render.
    async fn validate_csrf(&self) -> Result<(), PageError> {
        let token = self
            .request
            .str("__csrf__")
            .filter(|token| php::is_truthy_str(token))
            .or_else(|| {
                self.request
                    .header(axum::http::HeaderName::from_static("x-phabricator-csrf"))
            })
            .unwrap_or_default();
        let Some((secret, key)) = self.csrf().await? else {
            return Err(PageError::Fallback("a form post without a session"));
        };
        if csrf::is_valid_token(&secret, &key, token, self.state.now())? {
            Ok(())
        } else {
            Err(PageError::Fallback("an invalid CSRF token"))
        }
    }

    /// The `session.digest` key, loaded before anything is written.
    async fn session_digest(&self) -> Result<SessionDigest, PageError> {
        match SessionDigest::load(self.state.database()).await? {
            Outcome::Done(keys) => Ok(keys),
            Outcome::Unsupported(reason) => Err(PageError::Fallback(reason)),
        }
    }

    /// `PhabricatorController` refuses the `code` parameter on every page
    /// but the login callback, against redirection attacks.
    fn refuse_restricted_parameters(&self) -> Result<(), PageError> {
        if self.request.exists("code") {
            return Err(PageError::Fallback("a restricted parameter"));
        }
        Ok(())
    }
}

/// The answer of a login page: errors become `421`s or error pages.
fn respond(result: Result<Response, PageError>) -> Response {
    result.unwrap_or_else(axum::response::IntoResponse::into_response)
}
