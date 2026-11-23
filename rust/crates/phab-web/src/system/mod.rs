//! The read-only pages of the administration and infrastructure
//! applications: Applications, Config, Daemons, Conduit, Policy, Spaces,
//! `MetaMTA`, Help and Search, and the home page's menu (W14).
//!
//! They share one template ([`SystemPage`]): a header, an optional
//! navigation, sections of properties, tables or lists, and a side column
//! of actions, which link to PHP's forms. Every write stays with PHP.

pub(crate) mod applications;
pub(crate) mod catalog;
pub(crate) mod conduit;
pub(crate) mod config;
pub(crate) mod daemons;
pub(crate) mod help;
pub(crate) mod home;
pub(crate) mod mail;
pub(crate) mod policy;
pub(crate) mod search;
pub(crate) mod spaces;

use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::handler::Handler;
use axum::http::Uri;
use axum::routing::{self, MethodRouter};

use crate::diff_html::escape;
use crate::error::PageError;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

/// A link of the navigation or the side column. `inactive` is the current
/// page in the navigation, and a disabled action in the side column.
pub(crate) struct Link {
    pub(crate) name: String,
    pub(crate) uri: String,
    pub(crate) icon: Option<&'static str>,
    pub(crate) inactive: bool,
}

impl Link {
    pub(crate) fn new(name: impl Into<String>, uri: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            uri: uri.into(),
            icon: None,
            inactive: false,
        }
    }

    pub(crate) fn icon(mut self, icon: &'static str) -> Self {
        self.icon = Some(icon);
        self
    }

    pub(crate) fn inactive(mut self, inactive: bool) -> Self {
        self.inactive = inactive;
        self
    }
}

/// A property of a property list.
pub(crate) struct Property {
    pub(crate) label: String,
    pub(crate) value_html: String,
}

impl Property {
    /// A property with a plain-text value.
    pub(crate) fn text(label: impl Into<String>, value: &str) -> Self {
        Self::html(label, escape(value))
    }

    /// A property whose value is HTML built by the page.
    pub(crate) fn html(label: impl Into<String>, value_html: String) -> Self {
        Self {
            label: label.into(),
            value_html,
        }
    }
}

/// A table; its cells are HTML built by the page.
pub(crate) struct Table {
    pub(crate) headers: Vec<&'static str>,
    pub(crate) rows: Vec<Vec<String>>,
    pub(crate) empty: String,
}

/// An object of a list (`PHUIObjectItemView`).
#[derive(Default)]
pub(crate) struct Item {
    pub(crate) name: String,
    pub(crate) uri: Option<String>,
    pub(crate) icon: Option<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) attributes_html: Vec<String>,
    pub(crate) disabled: bool,
}

/// A list of objects.
pub(crate) struct List {
    pub(crate) items: Vec<Item>,
    pub(crate) empty: String,
}

/// What a section shows.
pub(crate) enum Body {
    Properties(Vec<Property>),
    Table(Table),
    List(List),
    /// HTML built by the page (rendered remarkup, escaped text).
    Html(String),
    Notice(String),
}

/// A section of the page.
pub(crate) struct Section {
    pub(crate) title: Option<String>,
    pub(crate) anchor: Option<&'static str>,
    pub(crate) body: Body,
}

impl Section {
    pub(crate) fn new(title: impl Into<String>, body: Body) -> Self {
        Self {
            title: Some(title.into()),
            anchor: None,
            body,
        }
    }

    pub(crate) fn untitled(body: Body) -> Self {
        Self {
            title: None,
            anchor: None,
            body,
        }
    }

    pub(crate) fn anchor(mut self, anchor: &'static str) -> Self {
        self.anchor = Some(anchor);
        self
    }
}

#[derive(Template)]
#[template(path = "system.html")]
pub(crate) struct SystemPage {
    shell: Shell,
    heading: String,
    icon: Option<String>,
    tags: Vec<String>,
    nav: Vec<Link>,
    sections: Vec<Section>,
    side: Vec<Link>,
}

/// Builds a [`SystemPage`].
pub(crate) struct PageBuilder {
    title: String,
    heading: String,
    icon: Option<String>,
    tags: Vec<String>,
    nav: Vec<Link>,
    sections: Vec<Section>,
    side: Vec<Link>,
}

impl PageBuilder {
    pub(crate) fn new(heading: impl Into<String>) -> Self {
        let heading = heading.into();
        Self {
            title: heading.clone(),
            heading,
            icon: None,
            tags: Vec::new(),
            nav: Vec::new(),
            sections: Vec::new(),
            side: Vec::new(),
        }
    }

    /// The page title, when it isn't the heading.
    pub(crate) fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    pub(crate) fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    pub(crate) fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }

    pub(crate) fn nav(mut self, nav: Vec<Link>) -> Self {
        self.nav = nav;
        self
    }

    pub(crate) fn section(mut self, section: Section) -> Self {
        self.sections.push(section);
        self
    }

    pub(crate) fn side(mut self, link: Link) -> Self {
        self.side.push(link);
        self
    }

    pub(crate) fn build<S: WebState>(
        self,
        state: &S,
        viewer: &WebUser,
        uri: &Uri,
    ) -> Page<SystemPage> {
        let current = uri.path_and_query().map_or("/", |path| path.as_str());
        Page(SystemPage {
            shell: Shell::for_user(state.config(), viewer, &self.title, current),
            heading: self.heading,
            icon: self.icon,
            tags: self.tags,
            nav: self.nav,
            sections: self.sections,
            side: self.side,
        })
    }
}

/// A read-only page: `GET` goes to `handler`, any other method (a form
/// posted to the page) to PHP.
pub(crate) fn read_only<H, T, S>(handler: H) -> MethodRouter<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    routing::get(handler).fallback(|| async { crate::fallback_response("a form") })
}

/// The routes of these pages.
pub(crate) fn routes<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route("/", read_only(home::show::<S>))
        .route("/applications/", read_only(applications::list::<S>))
        .route(
            "/applications/query/{key}/",
            read_only(applications::query::<S>),
        )
        .route(
            "/applications/view/{class}/",
            read_only(applications::view::<S>),
        )
        .route("/config/", read_only(config::console::<S>))
        .route("/config/settings/", read_only(config::settings::<S>))
        .route(
            "/config/settings/{filter}/",
            read_only(config::settings_filter::<S>),
        )
        .route("/config/edit/{key}/", read_only(config::option::<S>))
        .route("/daemon/", read_only(daemons::console::<S>))
        .route("/daemon/task/{id}/", read_only(daemons::task::<S>))
        .route("/daemon/bulk/", read_only(daemons::bulk::<S>))
        .route(
            "/daemon/bulk/query/{key}/",
            read_only(daemons::bulk_query::<S>),
        )
        .route("/conduit/", read_only(conduit::list::<S>))
        .route("/conduit/query/{key}/", read_only(conduit::query::<S>))
        .route("/conduit/method/{method}/", read_only(conduit::method::<S>))
        .route(
            "/policy/explain/{phid}/{capability}/",
            read_only(policy::explain::<S>),
        )
        .route("/spaces/", read_only(spaces::list::<S>))
        .route("/spaces/query/{key}/", read_only(spaces::query::<S>))
        .route("/mail/detail/{id}/", read_only(mail::detail::<S>))
        .route(
            "/help/documentation/{class}/",
            read_only(help::documentation::<S>),
        )
        .route(
            "/help/keyboardshortcut/",
            read_only(help::keyboard_shortcuts::<S>),
        )
        .route("/search/", read_only(search::search::<S>))
}

/// `shouldRequireAdmin()`: the page is for administrators, after the
/// application checks.
pub(crate) async fn require_admin<S: WebState>(
    state: &S,
    viewer: &WebUser,
    application: &'static str,
) -> Result<(), PageError> {
    viewer.require_application(state, application).await?;
    if !viewer.user.is_admin {
        return Err(PageError::PolicyDenied);
    }
    Ok(())
}

/// The query string must be empty: anything else is a search PHP builds
/// or a parameter Rust doesn't read.
pub(crate) fn no_query(uri: &Uri) -> Result<(), PageError> {
    match uri.query() {
        None | Some("") => Ok(()),
        Some(_) => Err(PageError::Fallback("a query string")),
    }
}

/// `PhutilNumber`: digits grouped by thousands.
pub(crate) fn thousands(number: i64) -> String {
    let digits = number.unsigned_abs().to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if number < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// A positive ID from the route, or the page doesn't exist.
pub(crate) fn parse_id(id: &str) -> Result<i64, PageError> {
    (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| id.parse().ok())
        .flatten()
        .ok_or(PageError::NotFound)
}

/// A remarkup text of PHP's own (descriptions of options, methods and
/// applications), rendered for the viewer.
pub(crate) async fn remarkup_html<S: WebState>(
    state: &S,
    viewer: &WebUser,
    text: &str,
) -> Result<String, PageError> {
    let markup = crate::remarkup::render(
        state,
        &viewer.user.viewer(),
        text,
        phab_remarkup::Mode::Html,
    )
    .await?;
    Ok(format!(
        "<div class=\"remarkup\">{}</div>",
        String::from_utf8_lossy(markup.as_bytes())
    ))
}

/// `phutil_format_units_generic()` with the units of durations: the
/// number in the largest unit (floored, as a float), and the remainder in
/// seconds.
#[allow(clippy::cast_precision_loss)]
fn duration_units(seconds: i64) -> (String, i64) {
    const SCALES: [i64; 4] = [60, 60, 24, 7];
    const LABELS: [&str; 5] = ["s", "m", "h", "d", "w"];
    let negative = seconds < 0;
    let mut number = seconds.unsigned_abs() as f64;
    let mut remainder = 0;
    let mut accumulated = 1;
    let mut label = 0;
    for scale in SCALES {
        if number < scale as f64 {
            break;
        }
        #[allow(clippy::cast_possible_truncation)]
        let whole = number as i64;
        remainder += (whole % scale) * accumulated;
        number /= scale as f64;
        accumulated *= scale;
        label += 1;
    }
    if negative {
        number = -number;
        remainder = -remainder;
    }
    #[allow(clippy::cast_possible_truncation)]
    let floored = number.floor() as i64;
    (format!("{floored} {}", LABELS[label]), remainder)
}

/// `phutil_format_relative_time()`, like `5 m`.
pub(crate) fn relative_time(seconds: i64) -> String {
    duration_units(seconds).0
}

/// `phutil_format_relative_time_detailed()` with two levels, like
/// `1 h, 5 m`.
pub(crate) fn relative_time_detailed(seconds: i64) -> String {
    if seconds == 0 {
        return "now".to_owned();
    }
    let (mut text, remainder) = duration_units(seconds.abs());
    if remainder > 0 {
        text.push_str(", ");
        text.push_str(&duration_units(remainder).0);
    }
    if seconds < 0 {
        text.push_str(" ago");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times_use_the_largest_unit() {
        assert_eq!(relative_time(0), "0 s");
        assert_eq!(relative_time(125), "2 m");
        assert_eq!(relative_time(-150), "-3 m");
        assert_eq!(relative_time(86_400 * 8), "1 w");
        assert_eq!(relative_time_detailed(3_900), "1 h, 5 m");
        assert_eq!(relative_time_detailed(-60), "1 m ago");
        assert_eq!(relative_time_detailed(0), "now");
    }
}
