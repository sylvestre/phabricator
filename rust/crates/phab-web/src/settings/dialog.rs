//! The dialogs of the settings actions, as PHP renders `newDialog()`
//! outside a workflow (a page with the form), and the high security
//! prompt they go through
//! (`PhabricatorAuthSessionEngine::requireHighSecuritySession()` and
//! `PhabricatorHighSecurityRequestExceptionHandler`).

use askama::Template;
use axum::http::StatusCode;
use axum::response::Response;
use phab_auth::Outcome;
use phab_auth::mfa::{self, FactorState, HighSecurity, HisecRequest, Mode, Planned};

use crate::error::PageError;
use crate::login::request::LoginRequest;
use crate::response;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

/// A paragraph, with an optional part in bold and the text after it.
pub(crate) struct Paragraph {
    pub(super) text: String,
    pub(super) strong: Option<String>,
    pub(super) tail: String,
}

impl Paragraph {
    pub(super) fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            strong: None,
            tail: String::new(),
        }
    }

    pub(super) fn bold(text: impl Into<String>, strong: impl Into<String>, tail: &str) -> Self {
        Self {
            text: text.into(),
            strong: Some(strong.into()),
            tail: tail.to_owned(),
        }
    }
}

/// The text field of a dialog.
pub(crate) struct TextField {
    pub(super) label: &'static str,
    pub(super) name: &'static str,
    pub(super) value: String,
    pub(super) error: Option<&'static str>,
    pub(super) read_only: bool,
}

/// A factor of the high security prompt.
pub(crate) struct FactorView {
    factor_name: String,
    input: Option<FactorInput>,
    wait: Option<String>,
    answered: bool,
}

/// The code a factor asks for.
pub(crate) struct FactorInput {
    name: String,
    value: String,
    error: Option<&'static str>,
}

/// The fields of the high security prompt (`renderHighSecurityForm()`).
pub(crate) struct HisecView {
    factors: Vec<FactorView>,
    answered_tokens: Option<String>,
}

#[derive(Template)]
#[template(path = "settings_dialog.html")]
pub(crate) struct DialogPage {
    shell: Shell,
    title: String,
    notice: Option<&'static str>,
    errors: Vec<String>,
    paragraphs: Vec<Paragraph>,
    field: Option<TextField>,
    hisec: Option<HisecView>,
    after: Vec<&'static str>,
    action: String,
    hidden: Vec<(String, String)>,
    submit: Option<&'static str>,
    cancel: Option<(String, &'static str)>,
}

/// A dialog to fill before [`Dialog::render()`].
pub(crate) struct Dialog {
    pub(super) title: String,
    pub(super) notice: Option<&'static str>,
    pub(super) errors: Vec<String>,
    pub(super) paragraphs: Vec<Paragraph>,
    pub(super) field: Option<TextField>,
    hisec: Option<HisecView>,
    pub(super) after: Vec<&'static str>,
    /// Where the form posts; the request's path by default.
    pub(super) action: Option<String>,
    pub(super) hidden: Vec<(String, String)>,
    pub(super) submit: Option<&'static str>,
    pub(super) cancel: Option<(String, &'static str)>,
}

impl Dialog {
    pub(super) fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            notice: None,
            errors: Vec::new(),
            paragraphs: Vec::new(),
            field: None,
            hisec: None,
            after: Vec::new(),
            action: None,
            hidden: Vec::new(),
            submit: None,
            cancel: None,
        }
    }

    pub(super) fn paragraph(mut self, text: impl Into<String>) -> Self {
        self.paragraphs.push(Paragraph::plain(text));
        self
    }

    pub(super) fn hidden(mut self, name: &str, value: impl Into<String>) -> Self {
        self.hidden.push((name.to_owned(), value.into()));
        self
    }

    pub(super) fn submit(mut self, label: &'static str) -> Self {
        self.submit = Some(label);
        self
    }

    /// `addCancelButton()`, labelled "Cancel" by default.
    pub(super) fn cancel(mut self, uri: impl Into<String>, label: &'static str) -> Self {
        self.cancel = Some((uri.into(), label));
        self
    }

    /// The page, with the cookies the request set.
    pub(super) fn render<S: WebState>(
        self,
        state: &S,
        viewer: &WebUser,
        request: LoginRequest,
    ) -> Response {
        let current = request
            .uri
            .path_and_query()
            .map_or("/", |path| path.as_str())
            .to_owned();
        let page = DialogPage {
            shell: Shell::for_user(state.config(), viewer, &self.title, &current),
            title: self.title,
            notice: self.notice,
            errors: self.errors,
            paragraphs: self.paragraphs,
            field: self.field,
            hisec: self.hisec,
            after: self.after,
            action: self.action.unwrap_or_else(|| request.uri.path().to_owned()),
            hidden: self.hidden,
            submit: self.submit,
            cancel: self.cancel,
        };
        request.finish(response::render(StatusCode::OK, &page))
    }
}

/// `isFormPost()`: a post of a form, not of the high security prompt,
/// with a valid CSRF token. PHP throws on a bad token: Rust leaves the
/// request to PHP.
pub(super) fn is_form_post<S: WebState>(
    state: &S,
    viewer: &WebUser,
    request: &LoginRequest,
) -> Result<bool, PageError> {
    if !(request.is_post() && request.exists("__form__") && !request.exists("__hisec__")) {
        return Ok(false);
    }
    require_csrf(state, viewer, request)?;
    Ok(true)
}

/// `isDialogFormPost()`: [`is_form_post()`] with `__dialog__`.
pub(super) fn is_dialog_form_post<S: WebState>(
    state: &S,
    viewer: &WebUser,
    request: &LoginRequest,
) -> Result<bool, PageError> {
    Ok(is_form_post(state, viewer, request)?
        && request
            .str("__dialog__")
            .is_some_and(phab_core::php::is_truthy_str))
}

/// `validateCSRF()`.
fn require_csrf<S: WebState>(
    state: &S,
    viewer: &WebUser,
    request: &LoginRequest,
) -> Result<(), PageError> {
    let token = request.str("__csrf__").unwrap_or_default();
    if viewer.is_valid_csrf_token(token, state.now())? {
        Ok(())
    } else {
        Err(PageError::Fallback("an invalid CSRF token"))
    }
}

/// The checks every action makes before writing: PHP writes nothing on
/// read-only installs.
pub(super) fn require_writable<S: WebState>(state: &S) -> Result<(), PageError> {
    if state.config().is_read_only() {
        return Err(PageError::Fallback("writing on a read-only install"));
    }
    Ok(())
}

/// What [`require_high_security()`] ends with.
pub(super) enum Hisec {
    /// The session is in high security, or needs no factor: the action
    /// goes on with the request.
    Granted(LoginRequest),
    /// The prompt, to answer with.
    Prompt(Response),
}

/// `requireHighSecuritySession($viewer, $request, $cancel_uri)`, with the
/// TOTP factors of the Rust engine: users with other factors, rate limits
/// and the other exception pages of PHP are left to PHP before anything
/// is written. Call it after every check which could leave the request
/// to PHP: what it writes (challenges, the session's high security) is
/// not undone.
pub(super) async fn require_high_security<S: WebState>(
    state: &S,
    viewer: &WebUser,
    request: LoginRequest,
    cancel_uri: &str,
) -> Result<Hisec, PageError> {
    if request.is_post() {
        require_csrf(state, viewer, &request)?;
    }
    let found = viewer.session_user();
    let now = state.now();
    let field = |name: &str| request.str(name).map(str::to_owned);
    let hisec_request = HisecRequest {
        is_post: request.is_post(),
        is_hisec_form: request.exists("__hisec__"),
        field: &field,
    };
    let mode = Mode {
        upgrade_session: true,
        jump_into_hisec: false,
        workflow_key: mfa::LEGACY_WORKFLOW,
    };
    let planned = match mfa::plan(state.database(), &found, &hisec_request, mode, now).await? {
        Outcome::Done(planned) => planned,
        Outcome::Unsupported(reason) => return Err(PageError::Fallback(reason)),
    };
    let plan = match planned {
        Planned::Token => return Ok(Hisec::Granted(request)),
        Planned::Challenge(plan) => plan,
    };
    require_writable(state)?;
    let info = request.info(state.hostname().unwrap_or_default());
    let (factors, answered) = match mfa::execute(state.database(), *plan, now, &info).await? {
        HighSecurity::Token => return Ok(Hisec::Granted(request)),
        HighSecurity::Prompt { factors, answered } => (factors, answered),
    };
    let is_wait = factors
        .iter()
        .any(|factor| matches!(factor.state, FactorState::Wait(_)));
    let is_continue = factors
        .iter()
        .any(|factor| factor.state == FactorState::Answered);
    let submit = if is_wait {
        "Wait Patiently"
    } else if is_continue {
        "Continue"
    } else {
        "Enter High Security"
    };
    let factors = factors
        .into_iter()
        .map(|prompt| {
            let name = prompt.factor.response_field();
            let mut view = FactorView {
                factor_name: prompt.factor.name.clone(),
                input: None,
                wait: None,
                answered: false,
            };
            match prompt.state {
                FactorState::Empty => {
                    view.input = Some(FactorInput {
                        name,
                        value: String::new(),
                        error: None,
                    });
                }
                FactorState::Rejected { value, error } => {
                    view.input = Some(FactorInput {
                        name,
                        value,
                        error: Some(error),
                    });
                }
                FactorState::Wait(reason) => view.wait = Some(reason),
                FactorState::Answered => view.answered = true,
            }
            view
        })
        .collect();
    let mut dialog = Dialog::new("Enter High Security");
    dialog.notice = Some("You are taking an action which requires you to enter high security.");
    dialog.paragraphs.push(Paragraph::plain(
        "To enter high security mode, confirm your credentials:",
    ));
    dialog.hisec = Some(HisecView {
        factors,
        answered_tokens: (!answered.is_empty()).then(|| answered.expose().to_owned()),
    });
    dialog.after.push(
        "Your account will remain in high security mode for a short period of time. When you \
         are finished taking sensitive actions, you should leave high security.",
    );
    // `getPassthroughRequestParameters()`: the request's own fields.
    dialog.hidden = request.passthrough_parameters();
    dialog.submit = Some(submit);
    dialog.cancel = Some((cancel_uri.to_owned(), "Cancel"));
    Ok(Hisec::Prompt(dialog.render(state, viewer, request)))
}
