//! `/settings/`: a user's own settings (`PhabricatorSettingsMainController`)
//! and the panels people use (W14).
//!
//! Ported: the panel list, the preference panels built on the edit engine
//! (Date and Time, Language, Diff and Display Preferences, Email Delivery
//! and Format) with their saves, Email Preferences (mail tags) with its
//! save, and the lists of email addresses, sessions, Conduit API tokens
//! and authentication factors. Saves write the rows PHP's
//! `PhabricatorUserPreferencesEditor` writes
//! ([`phab_app_settings::editor`]).
//!
//! Left to PHP (`421`): the other panels, every action of the list panels
//! (adding, verifying or removing addresses, MFA enrolment, tokens and
//! sessions), workflow dialogs, administrators editing bots and mailing
//! lists, the global defaults and the administrators' list of settings, the
//! translations of developer mode, and saves PHP would refuse (it shows
//! the errors on the form).

mod form;
mod lists;
mod mail;

use std::sync::Arc;

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use axum::{Router, routing};
use phab_app_settings::Error as SettingsError;
use phab_app_settings::preferences::{self, Preferences};
use phab_app_settings::setting::Context;
use phab_auth::User;
use phab_remarkup::Markup;
use serde_json::{Map, Value};

use crate::error::PageError;
use crate::login::request::LoginRequest;
use crate::response;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorSettingsApplication`.
const APPLICATION: &str = "PhabricatorSettingsApplication";

impl From<SettingsError> for PageError {
    fn from(error: SettingsError) -> Self {
        match error {
            SettingsError::Unsupported(reason) => Self::Fallback(reason),
            SettingsError::Db(error) => Self::Internal(Box::new(error)),
        }
    }
}

/// How Rust answers a panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A `PhabricatorEditEngineSettingsPanel` of settings.
    Form,
    /// `PhabricatorEmailPreferencesSettingsPanel`.
    MailTags,
    /// `PhabricatorEmailAddressesSettingsPanel`.
    EmailAddresses,
    /// `PhabricatorMultiFactorSettingsPanel`.
    Factors,
    /// `PhabricatorConduitTokensSettingsPanel`.
    ConduitTokens,
    /// `PhabricatorSessionsSettingsPanel`.
    Sessions,
    /// A panel left to PHP.
    Php,
}

/// A panel of the menu.
#[derive(Debug, Clone, Copy)]
struct Panel {
    key: &'static str,
    name: &'static str,
    icon: &'static str,
    kind: Kind,
}

const fn panel(key: &'static str, name: &'static str, icon: &'static str, kind: Kind) -> Panel {
    Panel {
        key,
        name,
        icon,
        kind,
    }
}

/// What decides which panels a user sees.
struct PanelFacts {
    has_password_provider: bool,
    allows_http_auth: bool,
    mails_each_recipient: bool,
}

/// `getAllDisplayPanels()` filtered by `buildPanels()` for a user editing
/// their own settings: the groups by `getPanelGroupOrder()`, their panels
/// by name, the panels for which `isUserPanel()` and `isEnabled()` hold.
fn panel_groups(facts: &PanelFacts) -> Vec<(&'static str, Vec<Panel>)> {
    let mut authentication = vec![
        panel("external", "External Accounts", "fa-users", Kind::Php),
        panel("multifactor", "Multi-Factor Auth", "fa-lock", Kind::Factors),
    ];
    if facts.has_password_provider {
        authentication.push(panel("password", "Password", "fa-key", Kind::Php));
    }
    authentication.push(panel("ssh", "SSH Public Keys", "fa-file-text-o", Kind::Php));
    if facts.allows_http_auth {
        authentication.push(panel("vcspassword", "VCS Password", "fa-code", Kind::Php));
    }
    let mut email = vec![
        panel("email", "Email Addresses", "fa-at", Kind::EmailAddresses),
        panel(
            "emaildelivery",
            "Email Delivery",
            "fa-envelope-o",
            Kind::Form,
        ),
    ];
    if facts.mails_each_recipient {
        email.push(panel("emailformat", "Email Format", "fa-font", Kind::Form));
    }
    email.push(panel(
        "emailpreferences",
        "Email Preferences",
        "fa-envelope-open-o",
        Kind::MailTags,
    ));
    vec![
        (
            "Account",
            vec![
                panel("datetime", "Date and Time", "fa-calendar", Kind::Form),
                panel("language", "Language", "fa-globe", Kind::Form),
            ],
        ),
        (
            "Applications",
            vec![
                panel("diff", "Diff Preferences", "fa-cog", Kind::Form),
                panel("display", "Display Preferences", "fa-desktop", Kind::Form),
                panel("editor", "External Editor", "fa-i-cursor", Kind::Php),
            ],
        ),
        ("Authentication", authentication),
        ("Email", email),
        (
            "Sessions and Logs",
            vec![
                panel("activity", "Activity Logs", "fa-list", Kind::Php),
                panel(
                    "apitokens",
                    "Conduit API Tokens",
                    "fa-tty",
                    Kind::ConduitTokens,
                ),
                panel("sessions", "Sessions", "fa-user", Kind::Sessions),
                panel("tokens", "Temporary Tokens", "fa-ticket", Kind::Php),
            ],
        ),
    ]
}

/// A link of the menu.
pub(crate) struct NavItem {
    name: &'static str,
    icon: &'static str,
    uri: String,
    selected: bool,
}

/// A group of the menu.
pub(crate) struct NavGroup {
    label: &'static str,
    items: Vec<NavItem>,
}

/// The content of a panel: exactly one is set.
#[derive(Default)]
pub(crate) struct Content {
    form: Option<form::FormView>,
    mail: Option<mail::MailView>,
    emails: Option<lists::EmailsView>,
    factors: Option<lists::FactorsView>,
    tokens: Option<lists::TokensView>,
    sessions: Option<lists::SessionsView>,
}

#[derive(Template)]
#[template(path = "settings.html")]
pub(crate) struct SettingsPage {
    shell: Shell,
    user_name: String,
    nav: Vec<NavGroup>,
    content: Content,
}

/// Markup as the HTML a template includes.
fn html(markup: &Markup) -> String {
    String::from_utf8_lossy(markup.as_bytes()).into_owned()
}

/// `phutil_escape_uri()`: `rawurlencode()`, keeping slashes.
fn escape_uri(text: &str) -> String {
    crate::login::request::rawurlencode(text).replace("%2F", "/")
}

/// The URI of a panel of `user`'s settings (`getPanelURI()`).
fn panel_uri(user_name: &str, key: &str) -> String {
    format!("/settings/user/{user_name}/page/{}/", escape_uri(key))
}

/// What a panel needs to answer.
struct PanelRequest<'a, S> {
    state: &'a S,
    viewer: &'a WebUser,
    request: LoginRequest,
    panel: Panel,
    page: SettingsPage,
}

impl<S: WebState> PanelRequest<'_, S> {
    /// The context of the settings' options.
    fn context(&self) -> Context<'_> {
        Context {
            config: self.state.config(),
            viewer_name: &self.viewer.user.user_name,
            now: self.state.now(),
        }
    }

    /// The URI of the panel.
    fn uri(&self) -> String {
        panel_uri(&self.viewer.user.user_name, self.panel.key)
    }

    /// The preferences of the viewer and the global defaults.
    async fn preferences(&self) -> Result<(Preferences, Option<Map<String, Value>>), PageError> {
        let database = self.state.database();
        let user = preferences::load(database, &self.viewer.user.phid).await?;
        let global = preferences::load_global(database).await?;
        Ok((user, global))
    }

    /// Checks the CSRF token of a form post (`isFormPost()`).
    fn require_csrf(&self) -> Result<(), PageError> {
        let token = self.request.str("__csrf__").unwrap_or_default();
        if self.viewer.is_valid_csrf_token(token, self.state.now())? {
            Ok(())
        } else {
            Err(PageError::BadRequest(
                "Your session expired; reload the page and try again.",
            ))
        }
    }

    /// What a save needs before writing: PHP sets a cookie or redirects on
    /// the request's domain, and writes nothing on read-only installs.
    fn require_writable(&self) -> Result<(), PageError> {
        if self.state.config().is_read_only() {
            return Err(PageError::Fallback(
                "saving settings on a read-only install",
            ));
        }
        if self.request.cookie_domain(self.state.config()).is_none() {
            return Err(PageError::Fallback("a request on a domain PHP refuses"));
        }
        Ok(())
    }

    /// The page with `content`, and the cookies the request set.
    fn render(mut self, content: Content) -> Response {
        self.page.content = content;
        let response = response::render(axum::http::StatusCode::OK, &self.page);
        self.request.finish(response)
    }
}

/// The settings routes.
pub(crate) fn routes<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route("/settings/", routing::get(list::<S>))
        .route(
            "/settings/panel/{panel}/",
            routing::get(panel_redirect::<S>),
        )
        .route(
            "/settings/user/{username}/",
            routing::get(user_settings::<S>).post(user_settings::<S>),
        )
        .route(
            "/settings/user/{username}/page/{key}/",
            routing::get(user_panel::<S>).post(user_panel::<S>),
        )
}

/// `GET /settings/`: users go to their own settings; administrators get
/// the list of every user's settings, which PHP shows.
pub(crate) async fn list<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    if viewer.user.is_admin {
        return Err(PageError::Fallback("the settings of every user"));
    }
    let request = LoginRequest::new(method, uri, headers, &body);
    let path = format!("/settings/user/{}/", viewer.user.user_name);
    Ok(request.redirect(state.config(), &path))
}

/// `GET /settings/panel/{panel}/`: a panel of the viewer's settings.
pub(crate) async fn panel_redirect<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let request = LoginRequest::new(method, uri, headers, &body);
    let path = panel_uri(&viewer.user.user_name, &key);
    Ok(request.redirect(state.config(), &path))
}

/// `/settings/user/{username}/`: the first panel.
pub(crate) async fn user_settings<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(username): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let request = LoginRequest::new(method, uri, headers, &body);
    show(state.as_ref(), &viewer, &username, None, request).await
}

/// `/settings/user/{username}/page/{key}/`.
pub(crate) async fn user_panel<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path((username, key)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let request = LoginRequest::new(method, uri, headers, &body);
    show(state.as_ref(), &viewer, &username, Some(&key), request).await
}

/// The user whose settings these are: PHP requires `CAN_VIEW` and
/// `CAN_EDIT` on them, which only the user has (administrators also edit
/// bots and mailing lists, which PHP handles).
async fn load_target<S: WebState>(
    state: &S,
    viewer: &WebUser,
    username: &str,
) -> Result<User, PageError> {
    let user = phab_auth::user::load_users_by_username(state.database(), &[username])
        .await?
        .into_iter()
        .next()
        .ok_or(PageError::NotFound)?;
    if user.phid != viewer.user.phid {
        if viewer.user.is_admin && (user.is_system_agent || user.is_mailing_list) {
            return Err(PageError::Fallback(
                "administrators editing the settings of a bot or mailing list",
            ));
        }
        return Err(PageError::PolicyDenied);
    }
    if user.is_system_agent || user.is_mailing_list {
        // Their preferences are editable by administrators only.
        return Err(PageError::Fallback("the settings of a bot or mailing list"));
    }
    Ok(user)
}

async fn show<S: WebState>(
    state: &S,
    viewer: &WebUser,
    username: &str,
    key: Option<&str>,
    request: LoginRequest,
) -> Result<Response, PageError> {
    viewer.require_application(state, APPLICATION).await?;
    let user = load_target(state, viewer, username).await?;
    if request.exists("__ajax__") || request.exists("__wflow__") {
        return Err(PageError::Fallback("a workflow request"));
    }
    let facts = PanelFacts {
        has_password_provider: phab_app_settings::account::has_password_provider(state.database())
            .await?,
        allows_http_auth: state.config().truthy("diffusion.allow-http-auth"),
        mails_each_recipient: state.config().truthy("metamta.one-mail-per-recipient"),
    };
    let groups = panel_groups(&facts);
    // `selectFilter()`: an unknown key shows the first panel.
    let panels = || groups.iter().flat_map(|(_, panels)| panels);
    let selected = key
        .and_then(|key| panels().find(|panel| panel.key == key))
        .or_else(|| panels().next())
        .copied()
        .ok_or(PageError::NotFound)?;
    if selected.kind == Kind::Php {
        return Err(PageError::Fallback("this settings panel"));
    }
    let nav = groups
        .iter()
        .map(|(label, panels)| NavGroup {
            label,
            items: panels
                .iter()
                .map(|panel| NavItem {
                    name: panel.name,
                    icon: panel.icon,
                    uri: panel_uri(&user.user_name, panel.key),
                    selected: panel.key == selected.key,
                })
                .collect(),
        })
        .collect();
    let current = request
        .uri
        .path_and_query()
        .map_or("/settings/", |path| path.as_str())
        .to_owned();
    let page = SettingsPage {
        shell: Shell::for_user(state.config(), viewer, selected.name, &current),
        user_name: user.user_name.clone(),
        nav,
        content: Content::default(),
    };
    let panel_request = PanelRequest {
        state,
        viewer,
        request,
        panel: selected,
        page,
    };
    match selected.kind {
        Kind::Form => form::handle(panel_request).await,
        Kind::MailTags => mail::handle(panel_request).await,
        Kind::EmailAddresses => lists::email_addresses(panel_request).await,
        Kind::Factors => lists::factors(panel_request).await,
        Kind::ConduitTokens => lists::conduit_tokens(panel_request).await,
        Kind::Sessions => lists::sessions(panel_request).await,
        Kind::Php => Err(PageError::Fallback("this settings panel")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(facts: &PanelFacts) -> Vec<&'static str> {
        panel_groups(facts)
            .into_iter()
            .flat_map(|(_, panels)| panels)
            .map(|panel| panel.key)
            .collect()
    }

    #[test]
    fn panels_depend_on_the_configuration() {
        let mut facts = PanelFacts {
            has_password_provider: false,
            allows_http_auth: false,
            mails_each_recipient: true,
        };
        assert_eq!(
            keys(&facts),
            [
                "datetime",
                "language",
                "diff",
                "display",
                "editor",
                "external",
                "multifactor",
                "ssh",
                "email",
                "emaildelivery",
                "emailformat",
                "emailpreferences",
                "activity",
                "apitokens",
                "sessions",
                "tokens"
            ]
        );
        facts.has_password_provider = true;
        facts.allows_http_auth = true;
        facts.mails_each_recipient = false;
        let keys = keys(&facts);
        assert!(keys.contains(&"password") && keys.contains(&"vcspassword"));
        assert!(!keys.contains(&"emailformat"));
    }

    #[test]
    fn panel_uris_escape_keys() {
        assert_eq!(
            panel_uri("alice", "a b/c"),
            "/settings/user/alice/page/a%20b/c/"
        );
    }
}
