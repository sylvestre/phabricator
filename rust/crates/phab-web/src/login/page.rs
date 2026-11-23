//! The pages of the login workflow: the login page, the password form,
//! the logout dialog and the error pages of `PhabricatorAuthController`.

use askama::Template;
use axum::http::StatusCode;
use axum::response::Response;

use crate::response;
use crate::shell::Shell;

/// A provider button of the login page: a link, where PHP renders a GET
/// form, which this page's `form-action 'self'` would refuse.
pub(super) struct LoginButton {
    pub label: &'static str,
    pub provider: &'static str,
    pub href: String,
}

/// The username and password form (`renderPasswordLoginForm()`).
pub(super) struct PasswordForm {
    pub action: String,
    pub csrf_token: String,
    pub username: String,
    pub allows_registration: bool,
}

/// One factor of the multi-factor prompt (`renderValidateFactorForm()`).
pub(super) struct FactorField {
    /// `Factor Name: ...`.
    pub factor_name: String,
    /// The code field, when the factor takes an answer.
    pub input: Option<FactorInput>,
    /// Why no answer can be taken now.
    pub wait: Option<String>,
    /// The factor was answered.
    pub answered: bool,
}

/// The code field of a factor.
pub(super) struct FactorInput {
    pub name: String,
    pub value: String,
    pub error: Option<&'static str>,
}

/// The multi-factor dialog of `/auth/finish/`.
pub(super) struct FactorForm {
    pub action: String,
    pub csrf_token: String,
    /// Who is logging in, on the login prompt.
    pub username: Option<String>,
    pub factors: Vec<FactorField>,
    /// `__hisec.challenges__`: response tokens of answered challenges.
    pub answered_tokens: Option<String>,
    pub cancel_uri: &'static str,
}

/// The account linking page (`PhabricatorAuthLinkController`).
pub(super) struct LinkForm {
    pub href: String,
    pub label: String,
}

/// The dialog confirming an account link
/// (`PhabricatorAuthConfirmLinkController`).
pub(super) struct ConfirmLink {
    pub action: String,
    pub csrf_token: String,
    pub account_name: String,
}

/// The registration form (`PhabricatorAuthRegisterController`).
pub(super) struct RegisterForm {
    pub action: String,
    pub csrf_token: String,
    pub provider: &'static str,
    pub account_name: String,
    pub username: String,
    pub username_error: Option<&'static str>,
    pub real_name: String,
    pub real_name_error: Option<&'static str>,
    /// The email field, when the user must choose an address.
    pub email: Option<String>,
    pub email_error: Option<&'static str>,
    pub email_caption: Option<String>,
}

/// The "Email Address Already in Use" dialog of the registration.
pub(super) struct EmailInUse {
    pub action: String,
    pub csrf_token: String,
    pub address: String,
}

#[derive(Template)]
#[template(path = "login.html")]
pub(super) struct LoginPage {
    pub shell: Shell,
    pub heading: String,
    /// Remarkup an administrator wrote (`auth.login`).
    pub custom_message: Option<phab_remarkup::Markup>,
    pub errors: Vec<String>,
    /// A message which is trusted HTML.
    pub message_html: Option<String>,
    pub buttons: Vec<LoginButton>,
    pub password: Option<PasswordForm>,
    pub email_login: bool,
    /// The CSRF token of the logout dialog, when the page is one.
    pub logout_csrf: Option<String>,
    /// The multi-factor prompt.
    pub factors: Option<FactorForm>,
    /// The link or refresh button.
    pub link: Option<LinkForm>,
    /// The account link confirmation.
    pub confirm_link: Option<ConfirmLink>,
    /// The registration form.
    pub register: Option<RegisterForm>,
    /// The dialog before registering without the external address.
    pub email_in_use: Option<EmailInUse>,
}

impl LoginPage {
    /// An empty page called `heading`.
    pub fn new(heading: &str) -> Self {
        Self {
            shell: Shell::bare(heading),
            heading: heading.to_owned(),
            custom_message: None,
            errors: Vec::new(),
            message_html: None,
            buttons: Vec::new(),
            password: None,
            email_login: false,
            logout_csrf: None,
            factors: None,
            link: None,
            confirm_link: None,
            register: None,
            email_in_use: None,
        }
    }

    /// `renderErrorPage()`: a title and messages.
    pub fn error(heading: &str, message: impl Into<String>) -> Self {
        let mut page = Self::new(heading);
        page.errors.push(message.into());
        page
    }

    fn custom_message(&self) -> Option<&str> {
        self.custom_message
            .as_ref()
            .map(|markup| std::str::from_utf8(markup.as_bytes()).unwrap_or_default())
    }

    /// The page with `status`.
    pub fn render(&self, status: StatusCode) -> Response {
        response::render(status, self)
    }
}

/// `buildProviderErrorResponse()`, as `renderError()` of the login
/// controller shows it.
pub(super) fn provider_error(provider: &str, message: &str) -> LoginPage {
    LoginPage::error(
        "Login Failed",
        format!(
            "Authentication provider (\"{provider}\") encountered an error while attempting \
             to log in. {message}"
        ),
    )
}
