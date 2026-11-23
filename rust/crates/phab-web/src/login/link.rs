//! Linking a BMO account to a logged-in user: `PhabricatorAuthLinkController`
//! (`/auth/link/<id>/`, `/auth/refresh/<id>/`), whose button goes through
//! Bugzilla back to the login callback, and
//! `PhabricatorAuthConfirmLinkController` (`/auth/confirmlink/<key>/`),
//! which links the account once the user passed multi-factor
//! authentication.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use phab_auth::external_account::{self, ExternalAccount};
use phab_auth::mfa::{self, FactorState, HighSecurity, HisecRequest, Mode, Planned};
use phab_auth::provider::{self, ProviderConfig, ProviderKind};
use phab_auth::session::SessionUser;
use phab_auth::{Outcome, User, oauth};
use phab_core::{Config, hash, random};

use super::page::{FactorField, FactorForm, FactorInput, LinkForm, LoginPage};
use super::{CLIENT_ID_COOKIE, Context, INVITE_COOKIE, REGISTRATION_COOKIE, Visitor, respond};
use crate::error::PageError;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorAuthApplication`, which the viewer must see.
const AUTH_APPLICATION: &str = "PhabricatorAuthApplication";
/// Where linking ends (`getSettingsURI()`).
pub(super) const PANEL_URI: &str = "/settings/panel/external/";
/// Length of the `phcid` cookie (`setClientIDCookie()`).
const CLIENT_ID_LENGTH: usize = 16;

/// What `/auth/{link|refresh}/<id>/` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Link,
    Refresh,
}

/// `GET /auth/link/<id>/`.
pub async fn link<S: WebState>(
    State(state): State<Arc<S>>,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    respond(
        show_link(
            state.as_ref(),
            Action::Link,
            &id,
            method,
            uri,
            headers,
            &body,
        )
        .await,
    )
}

/// `GET /auth/refresh/<id>/`.
pub async fn refresh<S: WebState>(
    State(state): State<Arc<S>>,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    respond(
        show_link(
            state.as_ref(),
            Action::Refresh,
            &id,
            method,
            uri,
            headers,
            &body,
        )
        .await,
    )
}

/// The checks of a controller which requires a login, then the login
/// context. Rust login being off is checked first, so those pages stay
/// PHP's whoever asks.
async fn logged_in_context<'a, S: WebState>(
    state: &'a S,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: &Bytes,
) -> Result<(Context<'a, S>, SessionUser), PageError> {
    if state.login().is_none() {
        return Err(PageError::Fallback("Rust login is off"));
    }
    let viewer = WebUser::from_headers(state, &headers).await?;
    viewer.require_application(state, AUTH_APPLICATION).await?;
    let context = Context::new(state, method, uri, headers, body).await?;
    context.refuse_restricted_parameters()?;
    let Visitor::LoggedIn { found } = &context.visitor else {
        return Err(PageError::Fallback("logged out"));
    };
    // The session `WebUser` checked; a race with a logout is PHP's.
    if found.session.id != viewer.session.id {
        return Err(PageError::Fallback("the session changed"));
    }
    let found = (**found).clone();
    Ok((context, found))
}

#[allow(clippy::too_many_arguments)]
async fn show_link<S: WebState>(
    state: &S,
    action: Action,
    id: &str,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: &Bytes,
) -> Result<Response, PageError> {
    // The route only matches digits.
    let Ok(id) = id.parse::<i64>() else {
        return Err(PageError::Fallback("a link URI without a provider ID"));
    };
    let (mut context, found) = logged_in_context(state, method, uri, headers, body).await?;
    let providers = match provider::load_providers(state.database()).await? {
        Outcome::Done(providers) => providers,
        Outcome::Unsupported(reason) => return Err(PageError::Fallback(reason)),
    };
    let Some(provider) = providers
        .iter()
        .find(|provider| provider.id == id && provider.is_enabled)
        .cloned()
    else {
        // A 404, or a provider class Rust doesn't know.
        return Err(PageError::Fallback("an unknown or disabled provider"));
    };
    if provider.kind != ProviderKind::Bmo {
        return Err(PageError::Fallback("a provider other than BMO"));
    }
    // BMO allows refreshing (`shouldAllowAccountRefresh()`).
    if action == Action::Link && !provider.allow_link {
        let page = LoginPage::error(
            "Account Not Linkable",
            "This provider is not configured to allow linking.",
        );
        return Ok(context.request.finish(page.render(StatusCode::OK)));
    }
    let has_account =
        external_account::user_has_account(state.database(), &provider, &found.user.phid).await?;
    match action {
        Action::Link if has_account => {
            let page = LoginPage::error(
                "Account Already Linked",
                "Your account is already linked to an external account for this provider.",
            );
            return Ok(context.request.finish(page.render(StatusCode::OK)));
        }
        Action::Refresh if !has_account => {
            let page = LoginPage::error(
                "No Account Linked",
                "You do not have a linked account on this provider, and thus can not refresh \
                 it.",
            );
            return Ok(context.request.finish(page.render(StatusCode::OK)));
        }
        _ => {}
    }

    // `setClientIDCookie()`, then the button with the state made from it.
    let config = context.config();
    let now = state.now();
    if context.request.nonempty_cookie(CLIENT_ID_COOKIE).is_none() {
        let client_id = random::random_characters(CLIENT_ID_LENGTH);
        context
            .request
            .set_temporary_cookie(config, CLIENT_ID_COOKIE, &client_id, now);
    }
    let client_id = context
        .request
        .nonempty_cookie(CLIENT_ID_COOKIE)
        .unwrap_or_default()
        .to_owned();
    let client_state = oauth::client_state(config, &client_id)?;
    let scope = context
        .request
        .str("scope")
        .filter(|scope| phab_core::php::is_truthy_str(scope))
        .unwrap_or(oauth::SCOPE)
        .to_owned();
    let app = super::provider::bmo_app(config, &provider)?;
    let (title, label) = match action {
        Action::Link => ("Link BMO Account", "Link External Account"),
        Action::Refresh => ("Refresh BMO Account", "Refresh Account Link"),
    };
    let mut page = LoginPage::new(title);
    page.link = Some(LinkForm {
        href: app.authenticate_uri(&scope, &client_state),
        label: label.to_owned(),
    });
    Ok(context.request.finish(page.render(StatusCode::OK)))
}

/// `loadAccountForRegistrationOrLinking()`: the account a registration or
/// link URI names, if this browser started that registration. The error
/// page PHP shows otherwise, or `Err` for paths left to PHP.
pub(super) enum LinkableAccount {
    /// The account, unlinked, with its provider.
    Found(Box<ExternalAccount>, ProviderConfig),
    /// An error page.
    Refused(&'static str),
}

pub(super) async fn load_linkable_account<S: WebState>(
    context: &Context<'_, S>,
    viewer: Option<&User>,
    account_key: &str,
) -> Result<LinkableAccount, PageError> {
    let database = context.state.database();
    let config = context.config();
    let account = match external_account::for_secret(database, account_key).await? {
        Outcome::Done(account) => account,
        Outcome::Unsupported(reason) => return Err(PageError::Fallback(reason)),
    };
    let Some(account) = account else {
        return Ok(LinkableAccount::Refused("No valid linkable account."));
    };
    if let Some(user_phid) = &account.user_phid {
        return Ok(LinkableAccount::Refused(
            if viewer.is_some_and(|viewer| viewer.phid == *user_phid) {
                "The account you are attempting to link is already linked to your account."
            } else {
                "The account you are attempting to register or link is already linked to \
                 another user."
            },
        ));
    }
    let Some(registration_key) = context
        .request
        .cookie(REGISTRATION_COOKIE)
        .filter(|key| phab_core::php::is_truthy_str(key))
    else {
        return Ok(LinkableAccount::Refused(
            "Your browser did not submit a registration key with the request. You must use the \
             same browser to begin and complete registration. Check that cookies are enabled and \
             try again.",
        ));
    };
    if !registration_key_matches(config, &account, registration_key)? {
        return Ok(LinkableAccount::Refused(
            "Your browser submitted a different registration key than the one associated with \
             this account. You may need to clear your cookies.",
        ));
    }
    let providers = match provider::load_providers(database).await? {
        Outcome::Done(providers) => providers,
        Outcome::Unsupported(reason) => return Err(PageError::Fallback(reason)),
    };
    // A disabled provider's error names it with `getDisplayName()`, PHP's.
    let Some(provider) = provider::by_phid(&providers, account.provider_config_phid())
        .filter(|provider| provider.is_enabled)
        .cloned()
    else {
        return Err(PageError::Fallback("a disabled or unknown provider"));
    };
    if provider.kind != ProviderKind::Bmo {
        return Err(PageError::Fallback("a provider other than BMO"));
    }
    Ok(LinkableAccount::Found(Box::new(account), provider))
}

/// Whether the `phreg` cookie is the key whose digest the account holds,
/// compared in constant time.
fn registration_key_matches(
    config: &Config,
    account: &ExternalAccount,
    registration_key: &str,
) -> Result<bool, PageError> {
    let expected = hash::weak_digest(registration_key.as_bytes(), config.hmac_key()?.as_bytes())?;
    // `getProperty()` of a value which is not a string compares unequal.
    let Some(actual) = account
        .property("registrationKey")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(false);
    };
    Ok(hash::hashes_are_identical(
        actual.as_bytes(),
        expected.as_bytes(),
    ))
}

/// `GET|POST /auth/confirmlink/<key>/`.
pub async fn confirm<S: WebState>(
    State(state): State<Arc<S>>,
    Path(account_key): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    respond(show_confirm(state.as_ref(), &account_key, method, uri, headers, &body).await)
}

async fn show_confirm<S: WebState>(
    state: &S,
    account_key: &str,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: &Bytes,
) -> Result<Response, PageError> {
    let (mut context, found) = logged_in_context(state, method, uri, headers, body).await?;
    let (mut account, provider) =
        match load_linkable_account(&context, Some(&found.user), account_key).await? {
            LinkableAccount::Found(account, provider) => (account, provider),
            LinkableAccount::Refused(message) => {
                let page = LoginPage::error("Authentication Error", message);
                return Ok(context.request.finish(page.render(StatusCode::OK)));
            }
        };
    if !provider.allow_link {
        let page = LoginPage::error("Authentication Error", "This account is not linkable.");
        return Ok(context.request.finish(page.render(StatusCode::OK)));
    }

    // `isFormOrHisecPost()`: a POST with `__form__` and a valid token.
    let is_post = context.request.is_post() && context.request.exists("__form__");
    if !is_post {
        let mut page = LoginPage::new("Confirm BMO Account Link");
        page.confirm_link = Some(super::page::ConfirmLink {
            action: context.request.uri.path().to_owned(),
            csrf_token: context.csrf_token().await?,
            account_name: account_name(&account),
        });
        return Ok(context.request.finish(page.render(StatusCode::OK)));
    }
    context.validate_csrf().await?;

    let account_phid = account
        .phid
        .clone()
        .ok_or(PageError::Fallback("an unsaved account"))?;
    let workflow_key = format!("account.link({})", account_phid.as_str());
    let now = state.now();
    let info = context.request.info(&context.settings.hostname);
    let request = &context.request;
    let field = |name: &str| request.str(name).map(str::to_owned);
    let hisec = HisecRequest {
        is_post: request.is_post(),
        is_hisec_form: request.exists("__hisec__"),
        field: &field,
    };
    let mode = Mode {
        upgrade_session: false,
        jump_into_hisec: false,
        workflow_key: &workflow_key,
    };
    let planned = match mfa::plan(state.database(), &found, &hisec, mode, now).await? {
        Outcome::Done(planned) => planned,
        Outcome::Unsupported(reason) => return Err(PageError::Fallback(reason)),
    };
    let outcome = match planned {
        Planned::Token => HighSecurity::Token,
        Planned::Challenge(plan) => mfa::execute(state.database(), *plan, now, &info).await?,
    };
    if let HighSecurity::Prompt { factors, answered } = outcome {
        let mut page = LoginPage::new("Provide MFA Credentials");
        page.errors.push(
            "You are taking an action which requires you to provide multi-factor credentials."
                .to_owned(),
        );
        page.factors = Some(factor_form(
            context.request.uri.path(),
            context.csrf_token().await?,
            None,
            factors,
            &answered,
            PANEL_URI,
        ));
        return Ok(context.request.finish(page.render(StatusCode::OK)));
    }

    account.user_phid = Some(found.user.phid.clone());
    external_account::save(state.database(), &mut account, now).await?;
    let config = context.config();
    context
        .request
        .clear_cookie(config, REGISTRATION_COOKIE, now);
    context.request.clear_cookie(config, CLIENT_ID_COOKIE, now);
    context.request.clear_cookie(config, INVITE_COOKIE, now);
    Ok(context.request.redirect(config, PANEL_URI))
}

/// How `PhabricatorAuthAccountView` names an account: the username, else
/// the real name, else the email address.
pub(super) fn account_name(account: &ExternalAccount) -> String {
    let profile = &account.profile;
    [&profile.username, &profile.real_name, &profile.email]
        .into_iter()
        .flatten()
        .find(|name| !name.is_empty())
        .cloned()
        .unwrap_or_default()
}

/// The fields of a multi-factor prompt (`renderHighSecurityForm()`).
pub(super) fn factor_form(
    action: &str,
    csrf_token: String,
    username: Option<String>,
    factors: Vec<mfa::PromptFactor>,
    answered: &mfa::AnsweredTokens,
    cancel_uri: &'static str,
) -> FactorForm {
    let fields = factors
        .into_iter()
        .map(|prompt| {
            let mut field = FactorField {
                factor_name: prompt.factor.name.clone(),
                input: None,
                wait: None,
                answered: false,
            };
            let name = prompt.factor.response_field();
            match prompt.state {
                FactorState::Empty => {
                    field.input = Some(FactorInput {
                        name,
                        value: String::new(),
                        error: None,
                    });
                }
                FactorState::Rejected { value, error } => {
                    field.input = Some(FactorInput {
                        name,
                        value,
                        error: Some(error),
                    });
                }
                FactorState::Wait(reason) => field.wait = Some(reason),
                FactorState::Answered => field.answered = true,
            }
            field
        })
        .collect();
    FactorForm {
        action: action.to_owned(),
        csrf_token,
        username,
        factors: fields,
        answered_tokens: (!answered.is_empty()).then(|| answered.expose().to_owned()),
        cancel_uri,
    }
}
