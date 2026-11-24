//! PHP's `ApplicationSearch` query form (`PhabricatorApplicationSearchController`),
//! shared by the list pages: the saved queries their results pages run
//! (`/<app>/query/<key>/`, a `search_savedquery` row) and the "Advanced
//! Search" form (`/<app>/query/advanced/[?query=<key>]`).
//!
//! Rust never saves a query: the form posts PHP's fields to PHP's path,
//! which Rust leaves to PHP (`421`), so PHP saves the query and redirects
//! to its results page, which Rust shows. Search parameters in a list's
//! URI save a query too, and stay with PHP.
//!
//! An engine's page maps the saved parameters to its query, and its form
//! fields to [`Field`]s; this module loads the queries, reads their
//! parameters as PHP's search fields do and renders the form. Left to PHP:
//! saved queries of another engine, queries the viewer named (their name
//! is the page's title) and parameters the engine doesn't map.

use askama::Template;
use axum::http::Uri;
use phab_conduit::MethodError;
use phab_core::Phid;
use phab_db::{DbError, rows};
use serde_json::{Map, Value};

use crate::error::PageError;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorApplicationSearchEngine`'s page size.
pub(crate) const PAGE_SIZE: usize = 100;

/// A `search_savedquery` row of an engine.
pub(crate) struct SavedQuery {
    /// `queryKey`.
    pub(crate) key: String,
    /// The decoded `parameters`.
    pub(crate) parameters: Map<String, Value>,
}

impl SavedQuery {
    /// An empty query, which the form shows without `?query=`.
    pub(crate) fn empty() -> Self {
        Self {
            key: String::new(),
            parameters: Map::new(),
        }
    }

    /// Loads the query `key` of `engine` for `viewer`: PHP answers 404 for
    /// a key nobody saved.
    pub(crate) async fn load<S: WebState>(
        state: &S,
        viewer: &WebUser,
        engine: &str,
        key: &str,
    ) -> Result<Self, PageError> {
        let database = state.database();
        let sql = format!(
            "SELECT engineClassName, parameters FROM {} WHERE queryKey = ?",
            database.table("search", "search_savedquery")
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(key)
            .fetch_optional(database.pool())
            .await
            .map_err(DbError::from)?
            .ok_or(PageError::NotFound)?;
        if rows::text(&row, "engineClassName")? != engine {
            return Err(PageError::Fallback(
                "saved queries of another search engine",
            ));
        }
        if is_named(state, &viewer.user.phid, engine, key).await? {
            return Err(PageError::Fallback("named saved queries"));
        }
        let parameters = match serde_json::from_str(&rows::text(&row, "parameters")?) {
            Ok(Value::Object(map)) => map,
            // PHP decodes an empty list as an empty map.
            Ok(Value::Array(list)) if list.is_empty() => Map::new(),
            _ => return Err(PageError::Fallback("unusual saved query parameters")),
        };
        Ok(Self {
            key: key.to_owned(),
            parameters,
        })
    }

    /// Fails unless every parameter is one of `known`, which the engine
    /// maps.
    pub(crate) fn only(&self, known: &[&str]) -> Result<(), PageError> {
        if self
            .parameters
            .keys()
            .all(|key| known.contains(&key.as_str()))
        {
            Ok(())
        } else {
            Err(PageError::Fallback("saved query parameters not ported"))
        }
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.parameters.get(key).filter(|value| !value.is_null())
    }

    /// A text field's value (`''` by default).
    pub(crate) fn text(&self, key: &str) -> Result<String, PageError> {
        match self.get(key) {
            None => Ok(String::new()),
            Some(Value::String(text)) => Ok(text.clone()),
            Some(Value::Number(number)) => Ok(number.to_string()),
            Some(_) => Err(PageError::Fallback("unusual saved query parameters")),
        }
    }

    /// A list field's value (`[]` by default): its string values.
    pub(crate) fn list(&self, key: &str) -> Result<Vec<String>, PageError> {
        let items: Vec<&Value> = match self.get(key) {
            None => return Ok(Vec::new()),
            Some(Value::Array(items)) => items.iter().collect(),
            Some(Value::Object(map)) => map.values().collect(),
            Some(_) => return Err(PageError::Fallback("unusual saved query parameters")),
        };
        items
            .into_iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or(PageError::Fallback("unusual saved query parameters"))
            })
            .collect()
    }

    /// A three-state field's value (`null` by default).
    pub(crate) fn three_state(&self, key: &str) -> Result<Option<bool>, PageError> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(_) => Err(PageError::Fallback("unusual saved query parameters")),
        }
    }

    /// An integer field's value (`null` by default).
    pub(crate) fn int(&self, key: &str) -> Result<Option<i64>, PageError> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Number(number)) => number
                .as_i64()
                .map(Some)
                .ok_or(PageError::Fallback("unusual saved query parameters")),
            Some(_) => Err(PageError::Fallback("unusual saved query parameters")),
        }
    }

    /// The `order` parameter, if any.
    pub(crate) fn order(&self) -> Result<Option<String>, PageError> {
        match self.get("order") {
            None => Ok(None),
            Some(Value::String(order)) => Ok(Some(order.clone())),
            Some(_) => Err(PageError::Fallback("order vectors")),
        }
    }

    /// `newPagerForSavedQuery()`: the `limit` parameter, at most 1000, or
    /// the engine's page size.
    pub(crate) fn page_size(&self) -> usize {
        let limit = match self.get("limit") {
            Some(Value::Number(number)) => number.as_i64().unwrap_or(0),
            Some(Value::String(text)) => int_cast(text),
            _ => 0,
        };
        if limit > 0 {
            usize::try_from(limit.min(1000)).unwrap_or(PAGE_SIZE)
        } else {
            PAGE_SIZE
        }
    }
}

/// `(int)` of a string: its leading digits, after whitespace and a sign.
fn int_cast(text: &str) -> i64 {
    let text = text.trim_start();
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    };
    let end = digits
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(digits.len());
    digits[..end].parse::<i64>().map_or(0, |value| sign * value)
}

/// Whether `viewer` (or everyone) named the query `key` of `engine`.
async fn is_named<S: WebState>(
    state: &S,
    viewer: &Phid,
    engine: &str,
    key: &str,
) -> Result<bool, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT 1 FROM {} WHERE engineClassName = ? AND queryKey = ? \
         AND userPHID IN (?, 'scope.global') LIMIT 1",
        database.table("search", "search_namedquery")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(engine)
        .bind(key)
        .bind(viewer.as_str())
        .fetch_optional(database.pool())
        .await
        .map_err(DbError::from)?
        .is_some())
}

/// Whether a token is a datasource function, like `viewer()`
/// (`PhabricatorTypeaheadDatasource::isFunctionToken()`).
pub(crate) fn is_function_token(token: &str) -> bool {
    token.ends_with(')') && token.contains('(')
}

/// A control of the form.
pub(crate) enum Control {
    /// A text input; tokenizers take names separated by commas, which
    /// PHP's `getListFromRequest()` reads as `getStrList()` does.
    Text(String),
    /// A select: the options' values and labels, and the selected value.
    Select(Vec<(String, String)>, String),
    /// Checkboxes posted as `key[]`: the options and the checked values.
    Checkboxes(Vec<(String, String)>, Vec<String>),
}

/// A field of the form.
pub(crate) struct Field {
    /// The input's name, the field's key.
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
    pub(crate) control: Control,
}

impl Field {
    /// A text field.
    pub(crate) fn text(key: &'static str, label: &'static str, value: String) -> Self {
        Self {
            key,
            label,
            control: Control::Text(value),
        }
    }

    /// A three-state select (`PhabricatorSearchThreeStateField`).
    pub(crate) fn three_state(
        key: &'static str,
        label: &'static str,
        options: [&str; 3],
        value: Option<bool>,
    ) -> Self {
        let selected = match value {
            None => "",
            Some(true) => "true",
            Some(false) => "false",
        };
        Self {
            key,
            label,
            control: Control::Select(
                ["", "true", "false"]
                    .into_iter()
                    .zip(options)
                    .map(|(value, label)| (value.to_owned(), label.to_owned()))
                    .collect(),
                selected.to_owned(),
            ),
        }
    }

    fn is_text(&self) -> bool {
        matches!(self.control, Control::Text(_))
    }

    fn text_value(&self) -> &str {
        match &self.control {
            Control::Text(value) => value,
            _ => "",
        }
    }

    fn options(&self) -> Vec<(&str, &str, bool)> {
        match &self.control {
            Control::Text(_) => Vec::new(),
            Control::Select(options, selected) => options
                .iter()
                .map(|(value, label)| (value.as_str(), label.as_str(), value == selected))
                .collect(),
            Control::Checkboxes(options, checked) => options
                .iter()
                .map(|(value, label)| (value.as_str(), label.as_str(), checked.contains(value)))
                .collect(),
        }
    }

    fn is_checkboxes(&self) -> bool {
        matches!(self.control, Control::Checkboxes(..))
    }
}

#[derive(Template)]
#[template(path = "search_form.html")]
pub(crate) struct AdvancedPage {
    shell: Shell,
    /// What the engine lists, like "Projects".
    results: &'static str,
    action: String,
    fields: Vec<Field>,
    /// The results page of the query the form edits.
    results_uri: Option<String>,
}

/// The "Advanced Search" form of an engine whose list is at `base` (like
/// `/project/`), filled with the query `saved`.
pub(crate) fn advanced_page<S: WebState>(
    state: &S,
    viewer: &WebUser,
    uri: &Uri,
    (results, base): (&'static str, &str),
    saved: &SavedQuery,
    fields: Vec<Field>,
) -> Page<AdvancedPage> {
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Page(AdvancedPage {
        shell: Shell::for_user(state.config(), viewer, "Advanced Search", current),
        results,
        action: uri.path().to_owned(),
        fields,
        results_uri: (!saved.key.is_empty()).then(|| format!("{base}query/{}/", saved.key)),
    })
}

/// An error of the query a saved query runs: PHP shows its own errors.
pub(crate) fn query_error(error: MethodError) -> PageError {
    match error {
        MethodError::Unsupported(reason) => PageError::Fallback(reason),
        MethodError::Conduit { .. } => PageError::Fallback("saved queries PHP refuses"),
        MethodError::Internal(error) => PageError::Internal(Box::new(std::io::Error::other(error))),
    }
}

/// The `?query=` of an advanced search URI, and nothing else.
pub(crate) fn edited_key(query: Option<&str>) -> Result<Option<String>, PageError> {
    let mut key = None;
    for (name, value) in form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
        match name.as_ref() {
            "query" => key = Some(value.into_owned()).filter(|key| !key.is_empty()),
            _ => return Err(PageError::Fallback("a search query")),
        }
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn saved(parameters: Value) -> SavedQuery {
        let Value::Object(parameters) = parameters else {
            panic!("parameters are a map");
        };
        SavedQuery {
            key: "abc".to_owned(),
            parameters,
        }
    }

    #[test]
    fn parameters_read_like_php_fields() {
        let query = saved(json!({
            "name": "build",
            "memberPHIDs": ["PHID-USER-a", "viewer()"],
            "isMilestone": false,
            "maxDepth": null,
            "limit": "2000",
        }));
        assert_eq!(query.text("name").unwrap(), "build");
        assert_eq!(query.text("other").unwrap(), "");
        assert_eq!(
            query.list("memberPHIDs").unwrap(),
            ["PHID-USER-a", "viewer()"]
        );
        assert_eq!(query.three_state("isMilestone").unwrap(), Some(false));
        assert_eq!(query.int("maxDepth").unwrap(), None);
        assert_eq!(query.page_size(), 1000);
        assert_eq!(int_cast(" 12abc"), 12);
        assert_eq!(int_cast("x"), 0);
        assert!(query.only(&["name"]).is_err());
        assert!(is_function_token("viewer()"));
        assert!(!is_function_token("PHID-USER-a"));
    }

    #[test]
    fn the_form_edits_a_query_by_key() {
        assert_eq!(
            edited_key(Some("query=abc")).unwrap().as_deref(),
            Some("abc")
        );
        assert_eq!(edited_key(None).unwrap(), None);
        assert!(edited_key(Some("name=x")).is_err());
    }
}
