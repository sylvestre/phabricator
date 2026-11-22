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
