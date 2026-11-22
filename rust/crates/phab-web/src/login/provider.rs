//! `PhabricatorAuthLoginController` (`/auth/login/<provider>/`): the BMO
//! OAuth callback and the password form, up to `loginUser()`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use phab_auth::external_account::{self, AccountProfile, ExternalAccount};
use phab_auth::oauth::{self, OAuthApp};
use phab_auth::password::{self, Password};
use phab_auth::provider::{self, ProviderConfig, ProviderKind};
use phab_auth::session::{self, SESSION_COOKIE, SessionDigest, SessionType};
use phab_auth::{Outcome, User, rate_limit};
use phab_core::{Config, Phid, hash, random};
use serde_json::{Value, json};

use super::page::{LoginPage, PasswordForm, provider_error};
use super::{
    CLIENT_ID_COOKIE, Context, INVITE_COOKIE, REGISTRATION_COOKIE, USERNAME_COOKIE, Visitor,
    respond,
};
use crate::error::PageError;
use crate::state::WebState;

/// Length of the registration key (`setAccountKeyAndContinue()`).
const REGISTRATION_KEY_LENGTH: usize = 32;

/// `PlatformSymbols::getPlatformServerName()`.
const SERVER_NAME: &str = "Phabricator";

/// `GET|POST /auth/login/{pkey}/`.
pub async fn login<S: WebState>(
    State(state): State<Arc<S>>,
    Path(provider_key): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    respond(handle(state.as_ref(), &provider_key, method, uri, headers, &body).await)
}

/// The BMO application of `provider`, as `configureAdapter()` sets it up.
pub(super) fn bmo_app(config: &Config, provider: &ProviderConfig) -> Result<OAuthApp, PageError> {
    let (Some(client_id), Some(client_secret)) = (
        provider.oauth_client_id.clone(),
        provider.oauth_client_secret.clone(),
    ) else {
        return Err(PageError::Fallback(
            "the BMO provider has no OAuth application",
        ));
    };
    let bugzilla_url = config
        .nonempty_string("bugzilla.url")
        .ok_or(PageError::Fallback("bugzilla.url is not set"))?;
    let redirect_uri = config
        .uri(&provider.kind.login_uri())
        .map_err(|_| PageError::Fallback("phabricator.base-uri is not set"))?;
    Ok(OAuthApp {
        bugzilla_url: bugzilla_url.to_owned(),
        client_id,
        client_secret,
        redirect_uri,
    })
}

async fn handle<S: WebState>(
    state: &S,
    provider_key: &str,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: &Bytes,
) -> Result<Response, PageError> {
    let mut context = Context::new(state, method, uri, headers, body).await?;
    // Linking, refreshing, partial sessions and disabled users.
    if matches!(context.visitor, Visitor::LoggedIn { .. }) {
        return Err(PageError::Fallback("logged in"));
    }
    if context.request.nonempty_cookie(INVITE_COOKIE).is_some() {
        return Err(PageError::Fallback("an invite"));
    }
    let providers = match provider::load_providers(state.database()).await? {
        Outcome::Done(providers) => providers,
        Outcome::Unsupported(reason) => return Err(PageError::Fallback(reason)),
    };
    let Some(provider) = provider::enabled_by_key(&providers, provider_key).cloned() else {
        return Err(PageError::Fallback("an unknown or disabled provider"));
    };
    let keys = context.session_digest().await?;
    match provider.kind {
        ProviderKind::Bmo => bmo_login(&mut context, &provider, &keys).await,
        ProviderKind::Password => password_login(&mut context, &provider, &keys).await,
    }
}

/// `PhabricatorBMOAuthProvider::processLoginRequest()` and the rest of the
/// controller.
async fn bmo_login<S: WebState>(
    context: &mut Context<'_, S>,
    provider: &ProviderConfig,
    keys: &SessionDigest,
) -> Result<Response, PageError> {
    let config = context.config();
    let app = bmo_app(config, provider)?;
    let request = &context.request;
    // Everything PHP refuses before the code is spent is PHP's to answer.
    if request
        .str("error")
        .is_some_and(phab_core::php::is_truthy_str)
    {
        return Err(PageError::Fallback("the OAuth provider returned an error"));
    }
    let Some(client_id) = request.nonempty_cookie(CLIENT_ID_COOKIE) else {
        return Err(PageError::Fallback("no client ID cookie"));
    };
    let state_param = request.str("state").unwrap_or_default();
    if !oauth::is_valid_state(config, client_id, state_param)? {
        return Err(PageError::Fallback("a wrong OAuth state"));
    }
    let Some(code) = request.nonempty_str("code").map(str::to_owned) else {
        return Err(PageError::Fallback("no OAuth code"));
    };
    context.issue_anonymous_session();

    let now = context.state.now();
    let settings = context.settings;
    let client = &settings.client;
    let grant = match oauth::exchange_code(client, &app, &code, now).await {
        Ok(grant) => grant,
        Err(error) => return Ok(exception(context, "exchanging the OAuth code", &error)),
    };
    let account = match oauth::load_account(client, &app.bugzilla_url, &grant.access_token).await {
        Ok(account) => account,
        Err(error) => return Ok(exception(context, "loading the BMO account", &error)),
    };
    let Some(identifier) = account.id() else {
        let page = provider_error(
            provider.kind.name(),
            "The OAuth provider failed to retrieve an account ID.",
        );
        return Ok(finish_page(context, &page));
    };

    let database = context.state.database();
    let Ok(mut external) =
        external_account::for_identifiers(database, provider, &[identifier]).await?
    else {
        return Ok(exception(
            context,
            "loading the external account",
            &"the account identifiers map to more than one account",
        ));
    };
    external.profile = AccountProfile {
        username: account.username(),
        real_name: account.real_name(),
        email: account.email(),
    };
    // `synchronizeOAuthAccount()`: BMO tokens can't be refreshed.
    external.set_property(
        "oauth.token.access",
        Value::String(grant.access_token.expose().to_owned()),
    );
    external.set_property("oauth.token.refresh", Value::Null);
    external.set_property("oauth.token.access.expires", json!(grant.expires));
    external_account::save(database, &mut external, now).await?;

    if config.truthy("bugzilla.require_mfa") && !account.has_mfa() {
        let bugzilla_url = config.string("bugzilla.url").unwrap_or_default();
        let mut page = LoginPage::new("Login Failed");
        page.message_html = Some(format!(
            "Authentication provider (\"{}\") encountered an error while attempting to log in. \
             Login using Bugzilla requires multi-factor authentication to be enabled in \
             Bugzilla. Please enable multi-factor authentication in your Bugzilla <a \
             href=\"{}\">preferences</a> and try again.",
            provider.kind.name(),
            escape_html(&format!("{bugzilla_url}/userprefs.cgi?tab=mfa")),
        ));
        return Ok(finish_page(context, &page));
    }
    continue_with_account(context, provider, keys, external).await
}

/// The part of `handleRequest()` after the provider returned an account,
/// for a logged-out visitor.
async fn continue_with_account<S: WebState>(
    context: &mut Context<'_, S>,
    provider: &ProviderConfig,
    keys: &SessionDigest,
    mut external: ExternalAccount,
) -> Result<Response, PageError> {
    let name = provider.kind.name();
    if let Some(user_phid) = external.user_phid.clone() {
        if !provider.allow_login {
            let page = LoginPage::error(
                "Login Failed",
                format!(
                    "The external service (\"{name}\") you just authenticated with is not \
                     configured to allow logins on this server. An administrator may have \
                     recently disabled it."
                ),
            );
            return Ok(finish_page(context, &page));
        }
        // `processLoginUser()`.
        let users =
            phab_auth::user::load_users_by_phid(context.state.database(), &[user_phid]).await?;
        let Some(user) = users.into_iter().next() else {
            let page = LoginPage::error(
                "Login Failed",
                format!(
                    "The external account you just logged in with is not associated with a \
                     valid {SERVER_NAME} user account."
                ),
            );
            return Ok(finish_page(context, &page));
        };
        return login_user(context, keys, &user).await;
    }
    if !provider.allows_registration() {
        let page = LoginPage::error(
            "Login Failed",
            format!(
                "The external service (\"{name}\") you just authenticated with is not \
                 configured to allow registration on this server. An administrator may have \
                 recently disabled it."
            ),
        );
        return Ok(finish_page(context, &page));
    }
    // `processRegisterUser()` and `setAccountKeyAndContinue()`: the
    // registration page itself is PHP's.
    let config = context.config();
    let now = context.state.now();
    let registration_key = random::random_characters(REGISTRATION_KEY_LENGTH);
    let digest = hash::weak_digest(registration_key.as_bytes(), config.hmac_key()?.as_bytes())?;
    external.set_property("registrationKey", Value::String(digest));
    external_account::save(context.state.database(), &mut external, now).await?;
    context
        .request
        .set_temporary_cookie(config, REGISTRATION_COOKIE, &registration_key, now);
    let secret = external.account_secret().unwrap_or_default().to_owned();
    Ok(take_request(context).redirect(config, &format!("/auth/register/{secret}/")))
}

/// `PhabricatorAuthController::loginUser()`: a partial session, the
/// username and session cookies, the registration cookies cleared, and
/// the redirect to `/auth/validate/`.
async fn login_user<S: WebState>(
    context: &mut Context<'_, S>,
    keys: &SessionDigest,
    user: &User,
) -> Result<Response, PageError> {
    let config = context.config();
    let now = context.state.now();
    let info = context.request.info(&context.settings.hostname);
    // Disabled users log in, and are stopped by every page afterwards.
    let issued = session::establish_session(
        context.state.database(),
        keys,
        SessionType::Web,
        &user.phid,
        true,
        now,
        &info,
    )
    .await?;
    let request = &mut context.request;
    request.set_cookie(config, USERNAME_COOKIE, &user.user_name, now);
    request.set_cookie(config, SESSION_COOKIE, issued.token(), now);
    request.clear_cookie(config, REGISTRATION_COOKIE, now);
    request.clear_cookie(config, CLIENT_ID_COOKIE, now);
    request.clear_cookie(config, INVITE_COOKIE, now);
    let target = format!(
        "/auth/validate/?expect={}",
        super::request::rawurlencode(&user.user_name)
    );
    Ok(take_request(context).redirect(config, &target))
}

/// Escapes text for an HTML attribute, as `phutil_tag()` does.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#039;")
}

/// The request out of `context`, to answer with its cookies.
fn take_request<S>(context: &mut Context<'_, S>) -> super::request::LoginRequest {
    std::mem::replace(
        &mut context.request,
        super::request::LoginRequest::new(
            Method::GET,
            Uri::from_static("/"),
            HeaderMap::new(),
            &Bytes::new(),
        ),
    )
}

/// An error page (`200`, as PHP's) with the cookies set so far.
fn finish_page<S>(context: &mut Context<'_, S>, page: &LoginPage) -> Response {
    take_request(context).finish(page.render(StatusCode::OK))
}

/// What PHP does when the exchange throws: an exception page. The OAuth
/// code is spent, so PHP can't answer instead.
fn exception<S>(
    context: &mut Context<'_, S>,
    what: &str,
    error: &dyn std::fmt::Display,
) -> Response {
    tracing::error!(%error, what, "BMO login failed");
    let page = LoginPage::error(
        "Unhandled Exception",
        "Logging in with BMO failed. Try again; if this keeps happening, contact an \
         administrator.",
    );
    take_request(context).finish(page.render(StatusCode::INTERNAL_SERVER_ERROR))
}

/// `PhabricatorUser::loadOneWithEmailAddress()` after a username lookup.
async fn find_user(
    database: &phab_db::Database,
    username_or_email: &str,
) -> Result<Option<User>, PageError> {
    let by_name = phab_auth::user::load_users_by_username(database, &[username_or_email]).await?;
    if let Some(user) = by_name.into_iter().next() {
        return Ok(Some(user));
    }
    let sql = format!(
        "SELECT userPHID FROM {} WHERE address = ? LIMIT 1",
        database.table("user", "user_email")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(username_or_email)
        .fetch_optional(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let phid = phab_db::rows::text(&row, "userPHID")?;
    Ok(
        phab_auth::user::load_users_by_phid(database, &[Phid::new(phid)])
            .await?
            .into_iter()
            .next(),
    )
}

/// `PhabricatorPasswordAuthProvider::processLoginRequest()`, for
/// administrators only: every other account is PHP's to log in.
async fn password_login<S: WebState>(
    context: &mut Context<'_, S>,
    provider: &ProviderConfig,
    keys: &SessionDigest,
) -> Result<Response, PageError> {
    context.refuse_restricted_parameters()?;
    let config = context.config();
    let database = context.state.database();
    let now = context.state.now();
    if config.truthy("recaptcha.enabled") {
        return Err(PageError::Fallback("CAPTCHAs are enabled"));
    }
    let actor = context.request.remote_address();
    if actor.is_empty() {
        return Err(PageError::Fallback("no remote address"));
    }
    if rate_limit::is_blocked(database, &actor, rate_limit::TRY_PASSWORD, now).await? {
        return Err(PageError::Fallback("too many login attempts"));
    }

    if context.request.is_post() {
        return admin_login(context, provider, keys, &actor).await;
    }

    // The form, as PHP shows it: the attempt counts, and the username
    // cookie is cleared.
    context.issue_anonymous_session();
    rate_limit::record(database, &actor, rate_limit::TRY_PASSWORD, 1.0, now).await?;
    context.request.clear_cookie(config, USERNAME_COOKIE, now);
    let username = context
        .request
        .nonempty_str("username")
        .or_else(|| context.request.nonempty_cookie(USERNAME_COOKIE))
        .unwrap_or_default()
        .to_owned();
    let mut page = LoginPage::new("Login");
    page.password = Some(PasswordForm {
        action: provider.kind.login_uri(),
        csrf_token: context.csrf_token().await?,
        username,
        allows_registration: provider.allows_registration(),
    });
    Ok(finish_page(context, &page))
}

/// A posted password form. Rust answers only an administrator typing
/// their right password; everything else, wrong passwords included, is
/// PHP's, so that no answer tells administrators from other accounts.
/// The password is checked for every existing account, so the time an
/// answer takes doesn't either.
async fn admin_login<S: WebState>(
    context: &mut Context<'_, S>,
    provider: &ProviderConfig,
    keys: &SessionDigest,
    actor: &str,
) -> Result<Response, PageError> {
    let database = context.state.database();
    let now = context.state.now();
    if !context.is_form_post().await? {
        return Err(PageError::Fallback("a POST which is not a form post"));
    }
    let username = context.request.str("username").unwrap_or_default();
    let user = if username.is_empty() {
        None
    } else {
        find_user(database, username).await?
    };
    let Some(user) = user else {
        return Err(PageError::Fallback("an unknown account"));
    };
    let typed = Password::new(context.request.str("password").unwrap_or_default());
    let is_valid = match password::is_valid_account_password(database, &user.phid, &typed).await? {
        Outcome::Done(is_valid) => is_valid,
        Outcome::Unsupported(reason) => return Err(PageError::Fallback(reason)),
    };
    if !is_valid || !user.is_admin {
        return Err(PageError::Fallback("not an administrator's password"));
    }
    if !provider.allow_login {
        return Err(PageError::Fallback("password login is disabled"));
    }
    let Ok(mut account) = external_account::for_user(database, provider, &user.phid).await? else {
        return Err(PageError::Fallback("several password accounts"));
    };

    // Writes start here: everything above can be replayed by PHP.
    rate_limit::record(database, actor, rate_limit::TRY_PASSWORD, 1.0, now).await?;
    // `didUpdateAccount()`: the password adapter provides no details.
    account.profile = AccountProfile::default();
    external_account::save(database, &mut account, now).await?;
    login_user(context, keys, &user).await
}
