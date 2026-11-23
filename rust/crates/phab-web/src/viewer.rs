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
use phab_core::PhidType;
use phab_policy::loader::{self, NoExtendedObjects};
use phab_policy::{ApplicationPolicies, Capability};

use crate::error::{PageError, Roadblock};
use crate::php::SessionToken;
use crate::state::WebState;

/// `PhabricatorAuthWaitForApprovalMessageType::MESSAGEKEY`.
const WAIT_FOR_APPROVAL: &str = "auth.wait-for-approval";

/// The viewer of a page: a logged-in, activated user with a full session,
/// enrolled in multi-factor authentication if the install requires it.
/// Logged-out viewers, unfinished logins and unverified email addresses
/// still go to PHP, which shows them the login or verification pages;
/// Rust has none of these yet (W7, W12).
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

    /// The checks of `PhabricatorController::willBeginExecution()` which
    /// depend on the page's application: it must be installed, and the
    /// viewer must see it and, when Spaces exist, at least one of them.
    pub async fn require_application<S: WebState>(
        &self,
        state: &S,
        class: &'static str,
    ) -> Result<(), PageError> {
        let config = state.config();
        let applications = ApplicationPolicies::from_config(config);
        // PHP has no routes for an uninstalled application.
        if !applications.is_installed(class) {
            return Err(PageError::NotFound);
        }
        let viewer = self.user.viewer();
        let subject = applications.subject(class, config.allow_public());
        let facts = loader::load_facts(
            state.database(),
            config,
            &viewer,
            &[&subject],
            &NoExtendedObjects,
            state.now(),
        )
        .await
        .map_err(|error| PageError::Internal(Box::new(error)))?;
        let internal = |error: phab_policy::PolicyError| PageError::Internal(Box::new(error));

        // `PhabricatorSpacesNoAccessController`.
        if !facts.spaces.is_empty() {
            let spaces: Vec<_> = facts.spaces.values().map(|space| &space.subject).collect();
            let visible = phab_policy::filter(&viewer, &spaces, &[Capability::VIEW], &facts)
                .map_err(internal)?;
            if !visible.contains(&true) {
                return Err(PageError::Roadblock(Roadblock::NoSpaces));
            }
        }
        if !phab_policy::filter::can_see_application(&viewer, class, &facts).map_err(internal)? {
            return Err(PageError::PolicyDenied);
        }
        Ok(())
    }

    /// `PhabricatorApplication::isClassInstalledForViewer()` of each of
    /// `classes`.
    pub async fn can_see_applications<S: WebState>(
        &self,
        state: &S,
        classes: &[&str],
    ) -> Result<Vec<bool>, PageError> {
        let applications = ApplicationPolicies::from_config(state.config());
        let visible = self.can_view_applications(state, classes).await?;
        Ok(classes
            .iter()
            .zip(visible)
            .map(|(class, visible)| visible && applications.is_installed(class))
            .collect())
    }

    /// Whether the viewer passes the view policy of each of `classes`,
    /// installed or not (the policy filter of `PhabricatorApplicationQuery`).
    pub async fn can_view_applications<S: WebState>(
        &self,
        state: &S,
        classes: &[&str],
    ) -> Result<Vec<bool>, PageError> {
        let config = state.config();
        let applications = ApplicationPolicies::from_config(config);
        let viewer = self.user.viewer();
        let subjects: Vec<_> = classes
            .iter()
            .map(|class| applications.subject(class, config.allow_public()))
            .collect();
        let subject_refs: Vec<_> = subjects.iter().collect();
        let facts = loader::load_facts(
            state.database(),
            config,
            &viewer,
            &subject_refs,
            &NoExtendedObjects,
            state.now(),
        )
        .await
        .map_err(|error| PageError::Internal(Box::new(error)))?;
        classes
            .iter()
            .map(|class| {
                phab_policy::filter::can_see_application(&viewer, class, &facts)
                    .map_err(|error| PageError::Internal(Box::new(error)))
            })
            .collect()
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
        Self::from_headers(state.as_ref(), &parts.headers).await
    }
}

impl WebUser {
    /// The viewer of a request with these headers, with the checks
    /// `PhabricatorController::willBeginExecution()` makes of controllers
    /// which require a login.
    pub async fn from_headers<S: WebState>(
        state: &S,
        headers: &HeaderMap,
    ) -> Result<Self, PageError> {
        let token = cookie(headers, SESSION_COOKIE)
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
        // In the order of `PhabricatorController::willBeginExecution()`.
        if found.user.is_disabled {
            return Err(PageError::Roadblock(Roadblock::Disabled));
        }
        if found.session.is_partial {
            return Err(PageError::Fallback(
                "multi-factor authentication is pending",
            ));
        }
        // `PhabricatorController::shouldRequireMultiFactorEnrollment()`.
        if config.truthy("security.require-multi-factor-auth")
            && found.user.phid.phid_type() == PhidType::USER
            && !phab_auth::user::is_enrolled_in_multi_factor(state.database(), &found.user.phid)
                .await?
        {
            return Err(PageError::Roadblock(Roadblock::NeedsMultiFactor));
        }
        // Sending another verification email needs PHP's mail (W12).
        if found.user.must_verify_email(config) {
            return Err(PageError::Fallback("the email address is not verified"));
        }
        if !found.user.is_approved {
            let text =
                phab_auth::user::load_message_text(state.database(), WAIT_FOR_APPROVAL).await?;
            let instructions = match text {
                Some(text) => Some(
                    crate::remarkup::render(
                        state,
                        &found.user.viewer(),
                        &text,
                        phab_remarkup::Mode::Html,
                    )
                    .await?,
                ),
                None => None,
            };
            return Err(PageError::Roadblock(Roadblock::NeedsApproval(instructions)));
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
