//! The logged-in user of a page, from the `phsid` session cookie
//! (`PhabricatorAuthSessionEngine::loadUserForSession()`, as called by
//! `AphrontApplicationConfiguration` for every web request).

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, header};
use phab_auth::csrf::{CsrfKey, CsrfSecret};
use phab_auth::session::{self, SESSION_COOKIE, Session, SessionType};
use phab_auth::{Outcome, User, Writes, csrf};

use crate::error::PageError;
use crate::php::SessionToken;
use crate::state::WebState;

/// The viewer of a page: a logged-in, activated user with a full session.
/// Everyone else goes to PHP, which shows them the login, approval or
/// multi-factor pages; Rust has none of these yet (W7).
#[derive(Debug, Clone)]
pub struct WebUser {
    /// The user.
    pub user: User,
    /// Their session.
    pub session: Session,
    /// A fresh CSRF token for the forms of the page.
    pub csrf_token: String,
    csrf_secret: CsrfSecret,
    csrf_key: CsrfKey,
    session_token: SessionToken,
}

impl WebUser {
    /// The session cookie, to call PHP as the user.
    pub fn session_token(&self) -> &SessionToken {
        &self.session_token
    }

    /// `PhabricatorUser::validateCSRFToken()`.
    pub fn is_valid_csrf_token(&self, token: &str, now: i64) -> Result<bool, PageError> {
        Ok(csrf::is_valid_token(
            &self.csrf_secret,
            &self.csrf_key,
            token,
            now,
        )?)
    }
}

/// The value of a cookie, if the request has it.
pub(crate) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

impl<S: WebState> FromRequestParts<Arc<S>> for WebUser {
    type Rejection = PageError;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<S>) -> Result<Self, PageError> {
        let token = cookie(&parts.headers, SESSION_COOKIE)
            .filter(|token| !token.is_empty())
            .ok_or(PageError::Fallback("logged out"))?;
        let config = state.config();
        let now = state.now();
        let writes = if config.is_read_only() {
            Writes::Skipped
        } else {
            Writes::Allowed
        };
        let found = match session::load_user_for_session(
            state.database(),
            config,
            SessionType::Web,
            token,
            now,
            writes,
        )
        .await?
        {
            Outcome::Done(Some(found)) => found,
            Outcome::Done(None) => return Err(PageError::Fallback("logged out")),
            Outcome::Unsupported(reason) => return Err(PageError::Fallback(reason)),
        };
        if found.session.is_partial {
            return Err(PageError::Fallback(
                "multi-factor authentication is pending",
            ));
        }
        if !found.user.is_activated(config) {
            return Err(PageError::Fallback("the account is not activated"));
        }
        let csrf_key = match csrf::load_key(state.database()).await? {
            Outcome::Done(key) => key,
            Outcome::Unsupported(reason) => return Err(PageError::Fallback(reason)),
        };
        let csrf_token = csrf::new_token(&found.csrf_secret, &csrf_key, now)?;
        Ok(Self {
            user: found.user,
            session: found.session,
            csrf_token,
            csrf_secret: found.csrf_secret,
            csrf_key,
            session_token: SessionToken(token.to_owned()),
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn cookies_are_found_in_any_header() {
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, HeaderValue::from_static("a=1; phsid=abc"));
        headers.append(header::COOKIE, HeaderValue::from_static("phab_ui=rust"));
        assert_eq!(cookie(&headers, "phsid"), Some("abc"));
        assert_eq!(cookie(&headers, "phab_ui"), Some("rust"));
        assert_eq!(cookie(&headers, "b"), None);
    }
}
