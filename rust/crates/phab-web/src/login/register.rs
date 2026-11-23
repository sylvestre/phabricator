//! `PhabricatorAuthRegisterController` (`/auth/register/<key>/`) for BMO
//! accounts: the username form, then the account, logged in.
//!
//! Registrations which make PHP write rows Rust doesn't are PHP's, decided
//! before anything is written: an address which isn't verified (PHP mails a
//! verification link), `auth.email-domains`, event listeners, invites,
//! `phabricator.production-uri`, an address another account already has
//! (PHP writes the user before failing on it), and every page PHP shows
//! without an account key (first-time setup, default providers).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use phab_auth::register::{self, ApprovalMail, NewUser};
use phab_auth::user::is_plain;

use super::link::{LinkableAccount, account_name, load_linkable_account};
use super::page::{EmailInUse, LoginPage, RegisterForm};
use super::{Context, INVITE_COOKIE, Visitor, respond};
use crate::error::PageError;
use crate::state::WebState;

/// `GET|POST /auth/register/<key>/`.
pub async fn register<S: WebState>(
    State(state): State<Arc<S>>,
    Path(account_key): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    respond(handle(state.as_ref(), &account_key, method, uri, headers, &body).await)
}

/// PHP's `(int)` of a request parameter (`getInt()`).
fn php_int(text: Option<&str>) -> i64 {
    let text = text.unwrap_or_default().trim_start();
    let end = text
        .char_indices()
        .find(|(index, character)| {
            !(character.is_ascii_digit() || (*index == 0 && matches!(character, '-' | '+')))
        })
        .map_or(text.len(), |(index, _)| index);
    text[..end].parse().unwrap_or(0)
}

fn error_page(message: impl Into<String>) -> LoginPage {
    LoginPage::error("Registration Failed", message)
}

#[allow(clippy::too_many_lines)]
async fn handle<S: WebState>(
    state: &S,
    account_key: &str,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: &Bytes,
) -> Result<Response, PageError> {
    let mut context = Context::new(state, method, uri, headers, body).await?;
    context.refuse_restricted_parameters()?;
    let config = context.config();
    match &context.visitor {
        Visitor::LoggedIn { found } if found.user.is_disabled => {
            return Err(PageError::Fallback("a disabled user"));
        }
        Visitor::LoggedIn { found } if found.session.is_partial => {
            return Err(PageError::Fallback("a partial session"));
        }
        Visitor::LoggedIn { .. } => return Ok(context.request.redirect(config, "/")),
        _ => {}
    }
    if context.request.nonempty_cookie(INVITE_COOKIE).is_some() || context.request.exists("invite")
    {
        return Err(PageError::Fallback("an invite"));
    }
    if config.truthy("events.listeners") {
        return Err(PageError::Fallback("event listeners"));
    }
    if config.truthy("auth.email-domains") {
        return Err(PageError::Fallback("auth.email-domains"));
    }
    context.issue_anonymous_session();

    let (mut account, provider) = match load_linkable_account(&context, None, account_key).await? {
        LinkableAccount::Found(account, provider) => (account, provider),
        LinkableAccount::Refused(message) => {
            return Ok(context
                .request
                .finish(error_page(message).render(StatusCode::OK)));
        }
    };
    if !provider.allows_registration() {
        let page = error_page(format!(
            "The account you are attempting to register with uses an authentication provider \
             (\"{}\") which does not allow registration. An administrator may have recently \
             disabled registration with this provider.",
            provider.kind.name()
        ));
        return Ok(context.request.finish(page.render(StatusCode::OK)));
    }

    let database = state.database();
    let mut errors: Vec<String> = Vec::new();
    let default_username = account.profile.username.clone().unwrap_or_default();
    let default_realname = account.profile.real_name.clone().unwrap_or_default();
    let mut default_email = account.profile.email.clone();
    if let Some(email) = default_email.clone()
        && !register::is_valid_address(&email)
    {
        errors.push(format!(
            "The email address associated with this external account (\"{email}\") is not a \
             valid email address and can not be used to register an account. Choose a \
             different, valid address."
        ));
        default_email = None;
    }
    // Another account has the address: register without it.
    let mut show_existing = None;
    if let Some(email) = default_email.clone()
        && register::email_exists(database, &email).await?
    {
        show_existing = Some(email);
        default_email = None;
    }
    let phase = php_int(context.request.str("phase"));
    if let Some(address) = &show_existing {
        if phase == 0 {
            let mut page = LoginPage::new("Email Address Already in Use");
            page.email_in_use = Some(EmailInUse {
                action: context.request.uri.path().to_owned(),
                csrf_token: context.csrf_token().await?,
                address: address.clone(),
            });
            return Ok(context.request.finish(page.render(StatusCode::OK)));
        }
        errors.push(format!(
            "The external account you are registering with has an email address that is \
             already in use (\"{address}\") by an existing Phabricator account. Choose a new, \
             valid email address to register a new account."
        ));
    }

    let can_edit_email = default_email.is_none();
    let require_real_name = config.truthy("user.require-real-name");
    let mut value_username = default_username.clone();
    let mut value_realname = default_realname.clone();
    let mut value_email = default_email.clone().unwrap_or_default();
    let mut e_username = None;
    let mut e_realname = None;
    let mut e_email = None;

    // `isFormPost()`; the username is always editable.
    let try_register = context.is_form_post().await? && phase != 1;
    if try_register {
        errors.clear();
        let request = &context.request;
        value_username = request.str("username").unwrap_or_default().to_owned();
        if value_username.is_empty() {
            e_username = Some("Required");
            errors.push("Username is required.".to_owned());
        } else if !register::is_valid_username(&value_username) {
            e_username = Some("Invalid");
            errors.push(register::VALID_USERNAME.to_owned());
        }
        if can_edit_email {
            value_email = request.str("email").unwrap_or_default().to_owned();
            if value_email.is_empty() {
                e_email = Some("Required");
                errors.push("Email is required.".to_owned());
            } else if !register::is_valid_address(&value_email) {
                e_email = Some("Invalid");
                errors.push(register::VALID_ADDRESSES.to_owned());
            }
        }
        // PHP would write a null real name.
        let Some(realname) = request.str("realName") else {
            return Err(PageError::Fallback(
                "a registration without a real name field",
            ));
        };
        value_realname = realname.to_owned();
        if value_realname.is_empty() && require_real_name {
            e_realname = Some("Required");
            errors.push("Real name is required.".to_owned());
        }

        if errors.is_empty() {
            // Only the address the provider gave can be verified.
            let email_verified =
                !can_edit_email && (account.email_verified || provider.trust_emails);
            if !email_verified {
                return Err(PageError::Fallback("an address to verify by mail"));
            }
            if !is_plain(&value_username) || !is_plain(&value_realname) {
                return Err(PageError::Fallback("a name which is not printable ASCII"));
            }
            let must_approve = config.truthy("auth.require-approval");
            let approval_mail = if must_approve {
                let (Some(queue_uri), Some(config_uri)) = (
                    register::production_uri(config, "/people/query/approval/"),
                    register::production_uri(config, "/config/edit/auth.require-approval/"),
                ) else {
                    return Err(PageError::Fallback("phabricator.production-uri"));
                };
                Some(ApprovalMail {
                    admins: register::load_admins(database).await?,
                    queue_uri,
                    config_uri,
                })
            } else {
                None
            };
            // PHP's insert fails on a taken username before writing; on a
            // taken address, after writing the user's search task.
            let same_username = register::username_exists(database, &value_username).await?;
            let same_email = register::email_exists(database, &value_email).await?;
            if !same_username && same_email {
                return Err(PageError::Fallback("an address another account has"));
            }
            if same_username {
                e_username = Some("Duplicate");
                errors.push("Another user already has that username.".to_owned());
                if same_email {
                    e_email = Some("Duplicate");
                    errors.push("Another user already has that email.".to_owned());
                }
            } else {
                let keys = context.session_digest().await?;
                let now = state.now();
                let new_user = NewUser {
                    username: value_username,
                    real_name: value_realname,
                    email: value_email,
                    email_verified,
                    approved: !must_approve,
                };
                let user = register::create(
                    database,
                    &new_user,
                    &mut account,
                    approval_mail.as_ref(),
                    now,
                )
                .await?;
                return super::provider::login_user(&mut context, &keys, &user).await;
            }
        }
    }

    let mut page = LoginPage::new("Create a New Account");
    page.errors = errors;
    page.register = Some(RegisterForm {
        action: context.request.uri.path().to_owned(),
        csrf_token: context.csrf_token().await?,
        provider: provider.kind.name(),
        account_name: account_name(&account),
        username: value_username,
        username_error: e_username,
        real_name: value_realname,
        real_name_error: e_realname,
        email: can_edit_email.then_some(value_email),
        email_error: e_email,
        email_caption: None,
    });
    Ok(context.request.finish(page.render(StatusCode::OK)))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(None, 0)]
    #[case(Some("1"), 1)]
    #[case(Some("2abc"), 2)]
    #[case(Some(" 2"), 2)]
    #[case(Some("x"), 0)]
    #[case(Some("-1"), -1)]
    fn ints_cast_like_php(#[case] text: Option<&str>, #[case] expected: i64) {
        assert_eq!(php_int(text), expected);
    }
}
