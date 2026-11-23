//! The actions of `PhabricatorEmailAddressesSettingsPanel`: adding an
//! address (`?new`), deleting one (`?delete`), sending another
//! verification mail (`?verify`) and making one primary (`?primary`), with
//! their dialogs and PHP's high security rules (adding, deleting and
//! making primary require it).
//!
//! Every write goes through [`phab_auth::email`], which writes PHP's rows.
//! Left to PHP (`421`, before writing): `auth.email-domains`, addresses PHP
//! refuses (its form shows the error), addresses another account has,
//! rate limits, links built from `phabricator.production-uri`, users whose
//! account isn't email verified making an address primary, and removals
//! the destruction engine's extensions would write more for.

use axum::response::Response;
use phab_auth::Outcome;
use phab_auth::email::{self, Actor, Added, Owner, UserEmail, Which};
use phab_core::php;

use super::PanelRequest;
use super::dialog::{self, Dialog, Hisec, Paragraph, TextField};
use crate::error::PageError;
use crate::login::request::LoginRequest;
use crate::state::WebState;
use crate::viewer::WebUser;

/// What the panel's actions share.
struct Action<'a, S> {
    state: &'a S,
    viewer: &'a WebUser,
    /// `$request->getPath()`, where the dialogs post and go back to.
    uri: String,
    /// `getPanelURI()`, where the prompt's cancel button goes.
    panel_uri: String,
}

impl<S: WebState> Action<'_, S> {
    fn actor(&self) -> Actor<'_> {
        Actor {
            phid: &self.viewer.user.phid,
            session_digest: &self.viewer.session.key_digest,
        }
    }

    fn owner(&self) -> Owner<'_> {
        Owner {
            phid: &self.viewer.user.phid,
            username: &self.viewer.user.user_name,
        }
    }

    async fn require_high_security(&self, request: LoginRequest) -> Result<Hisec, PageError> {
        dialog::require_high_security(self.state, self.viewer, request, &self.panel_uri).await
    }
}

/// `trim()`.
fn php_trim(text: &str) -> &str {
    text.trim_matches([' ', '\t', '\n', '\r', '\0', '\u{b}'])
}

/// `getInt()` of a parameter, `0` when it is missing.
fn int(request: &LoginRequest, name: &str) -> i64 {
    request
        .str(name)
        .map_or(0, phab_app_settings::preferences::int_cast_str)
}

/// `processRequest()` before the list: the action the request asks for,
/// in PHP's order, or the panel back when it asks for the list.
pub(super) async fn dispatch<S: WebState>(
    panel: PanelRequest<'_, S>,
) -> Result<Result<Response, PanelRequest<'_, S>>, PageError> {
    let editable = panel.state.config().truthy("account.editable");
    let new = panel
        .request
        .str("new")
        .filter(|new| php::is_truthy_str(new))
        .map(str::to_owned);
    let delete = int(&panel.request, "delete");
    let verify = int(&panel.request, "verify");
    let primary = int(&panel.request, "primary");
    let wants = if editable && new.is_some() {
        Wants::New(new.unwrap_or_default())
    } else if editable && delete != 0 {
        Wants::Delete(delete)
    } else if verify != 0 {
        Wants::Verify(verify)
    } else if primary != 0 {
        Wants::Primary(primary)
    } else {
        return Ok(Err(panel));
    };
    let action = Action {
        state: panel.state,
        viewer: panel.viewer,
        uri: panel.request.uri.path().to_owned(),
        panel_uri: panel.uri(),
    };
    let request = panel.request;
    let response = match wants {
        Wants::New(new) => new_address(&action, request, &new).await?,
        Wants::Delete(id) => delete_address(&action, request, id).await?,
        Wants::Verify(id) => verify_address(&action, request, id).await?,
        Wants::Primary(id) => primary_address(&action, request, id).await?,
    };
    Ok(Ok(response))
}

/// The action a request asks for.
enum Wants {
    New(String),
    Delete(i64),
    Verify(i64),
    Primary(i64),
}

/// `returnNewAddressResponse()`.
async fn new_address<S: WebState>(
    action: &Action<'_, S>,
    request: LoginRequest,
    new: &str,
) -> Result<Response, PageError> {
    let (state, viewer) = (action.state, action.viewer);
    if state
        .config()
        .value("auth.email-domains")
        .is_some_and(php::is_truthy)
    {
        return Err(PageError::Fallback("auth.email-domains"));
    }
    let request = match action.require_high_security(request).await? {
        Hisec::Granted(request) => request,
        Hisec::Prompt(response) => return Ok(response),
    };
    if dialog::is_dialog_form_post(state, viewer, &request)? {
        let address = php_trim(request.str("email").unwrap_or_default()).to_owned();
        if new == "verify" {
            // "Done" on the "an email has been sent" dialog.
            return Ok(request.redirect(state.config(), &action.uri));
        }
        dialog::require_writable(state)?;
        let now = state.now();
        if email::is_add_rate_limited(state.database(), &viewer.user.phid, now).await? {
            return Err(PageError::Fallback("adding addresses too quickly"));
        }
        if address.is_empty() || !phab_auth::register::is_valid_address(&address) {
            // PHP counts the attempt and shows the error on the form.
            return Err(PageError::Fallback("an address PHP refuses"));
        }
        if email::address_exists(state.database(), &address).await? {
            return Err(PageError::Fallback("an address another account has"));
        }
        if email::verification_link(state.config(), "code").is_none() {
            return Err(PageError::Fallback("a verification link PHP makes"));
        }
        let info = request.info(state.hostname().unwrap_or_default());
        let added = email::add(
            state.database(),
            action.actor(),
            &viewer.user.phid,
            &address,
            &info,
            now,
        )
        .await?;
        let added = match added {
            Added::Added(added) => added,
            Added::Duplicate => return Err(PageError::Fallback("an address another account has")),
        };
        if let Outcome::Unsupported(reason) = email::send_verification(
            state.database(),
            state.config(),
            action.owner(),
            &added,
            now,
        )
        .await?
        {
            // Checked above; the address is written, so this is an error.
            return Err(PageError::Internal(reason.into()));
        }
        let mut dialog = Dialog::new("Verification Email Sent")
            .hidden("new", "verify")
            .paragraph(
                "A verification email has been sent. Click the link in the email to verify \
                 your address.",
            )
            .submit("Done");
        dialog.action = Some(action.uri.clone());
        return Ok(dialog.render(state, viewer, request));
    }
    // The field is empty: what a dialog post sends is either saved or
    // refused by PHP.
    let mut dialog = Dialog::new("New Address")
        .hidden("new", "true")
        .submit("Save")
        .cancel(action.uri.clone(), "Cancel");
    dialog.field = Some(TextField {
        label: "Email",
        name: "email",
        value: String::new(),
        error: None,
        read_only: false,
    });
    Ok(dialog.render(state, viewer, request))
}

/// `returnDeleteAddressResponse()`.
async fn delete_address<S: WebState>(
    action: &Action<'_, S>,
    request: LoginRequest,
    id: i64,
) -> Result<Response, PageError> {
    let (state, viewer) = (action.state, action.viewer);
    let request = match action.require_high_security(request).await? {
        Hisec::Granted(request) => request,
        Hisec::Prompt(response) => return Ok(response),
    };
    let email = load(state, viewer, id, Which::NotPrimary).await?;
    if dialog::is_form_post(state, viewer, &request)? {
        dialog::require_writable(state)?;
        if email::removal_has_side_effects(state.database(), &email).await? {
            return Err(PageError::Fallback("an address other objects refer to"));
        }
        let info = request.info(state.hostname().unwrap_or_default());
        let removed = email::remove(
            state.database(),
            action.actor(),
            &viewer.user.phid,
            &email,
            &info,
            state.now(),
        )
        .await?;
        if let Outcome::Unsupported(reason) = removed {
            return Err(PageError::Fallback(reason));
        }
        return Ok(request.redirect(state.config(), &action.uri));
    }
    let dialog = Dialog::new(format!("Really delete address '{}'?", email.address))
        .hidden("delete", id.to_string())
        .paragraph(
            "Are you sure you want to delete this address? You will no longer be able to use \
             it to login.",
        )
        .paragraph(
            "Note: Removing an email address from your account will invalidate any \
             outstanding password reset links.",
        )
        .submit("Delete")
        .cancel(action.uri.clone(), "Cancel");
    Ok(dialog.render(state, viewer, request))
}

/// `returnVerifyAddressResponse()`: no high security.
async fn verify_address<S: WebState>(
    action: &Action<'_, S>,
    request: LoginRequest,
    id: i64,
) -> Result<Response, PageError> {
    let (state, viewer) = (action.state, action.viewer);
    let email = load(state, viewer, id, Which::Unverified).await?;
    if dialog::is_form_post(state, viewer, &request)? {
        dialog::require_writable(state)?;
        let sent = email::send_verification(
            state.database(),
            state.config(),
            action.owner(),
            &email,
            state.now(),
        )
        .await?;
        if let Outcome::Unsupported(reason) = sent {
            return Err(PageError::Fallback(reason));
        }
        return Ok(request.redirect(state.config(), &action.uri));
    }
    let mut dialog = Dialog::new("Send Another Verification Email?")
        .hidden("verify", id.to_string())
        .submit("Send Email")
        .cancel(action.uri.clone(), "Cancel");
    dialog.paragraphs.push(Paragraph::plain(format!(
        "Send another copy of the verification email to {}?",
        email.address
    )));
    Ok(dialog.render(state, viewer, request))
}

/// `returnPrimaryAddressResponse()`.
async fn primary_address<S: WebState>(
    action: &Action<'_, S>,
    request: LoginRequest,
    id: i64,
) -> Result<Response, PageError> {
    let (state, viewer) = (action.state, action.viewer);
    let request = match action.require_high_security(request).await? {
        Hisec::Granted(request) => request,
        Hisec::Prompt(response) => return Ok(response),
    };
    let email = load(state, viewer, id, Which::VerifiedNotPrimary).await?;
    if dialog::is_form_post(state, viewer, &request)? {
        dialog::require_writable(state)?;
        if !viewer.user.is_email_verified {
            // PHP also saves the user, verified.
            return Err(PageError::Fallback("an account which isn't email verified"));
        }
        let info = request.info(state.hostname().unwrap_or_default());
        let changed = email::make_primary(
            state.database(),
            action.actor(),
            action.owner(),
            &email,
            &info,
            state.now(),
        )
        .await?;
        if let Outcome::Unsupported(reason) = changed {
            return Err(PageError::Fallback(reason));
        }
        return Ok(request.redirect(state.config(), &action.uri));
    }
    let dialog = Dialog::new("Change primary email address?")
        .hidden("primary", id.to_string())
        .paragraph(format!(
            "If you change your primary address, Phabricator will send all email to {}.",
            email.address
        ))
        .paragraph(
            "Note: Changing your primary email address will invalidate any outstanding \
             password reset links.",
        )
        .submit("Change Primary Address")
        .cancel(action.uri.clone(), "Cancel");
    Ok(dialog.render(state, viewer, request))
}

/// The viewer's address `id`, `404` when the action can't take it.
async fn load<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: i64,
    which: Which,
) -> Result<UserEmail, PageError> {
    email::load(state.database(), id, &viewer.user.phid, which)
        .await?
        .ok_or(PageError::NotFound)
}
