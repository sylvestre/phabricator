//! `/flag/` (`PhabricatorFlagListController`): the viewer's flags, the
//! "Flagged" query of `PhabricatorFlagSearchEngine`, loaded as `flag.query`
//! loads them. Editing and removing flags link to PHP's workflows. Saved
//! queries, filters (types, colors, grouping) and more than a page of
//! flags go to PHP.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use phab_conduit::Method as _;
use serde_json::{Value, json};

use crate::error::PageError;
use crate::harbormaster::{Scope, method_error};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::time;
use crate::viewer::WebUser;

/// `PhabricatorFlagsApplication`.
const APPLICATION: &str = "PhabricatorFlagsApplication";

/// `PhabricatorApplicationSearchEngine` shows this many results a page.
const PAGE_SIZE: usize = 100;

/// A flag in the list.
pub(crate) struct FlagItem {
    id: String,
    object_phid: String,
    name: String,
    uri: String,
    /// `PhabricatorFlagColor::getCSSClass()`, without its prefix.
    color: String,
    color_name: String,
    note: String,
    date: String,
}

#[derive(Template)]
#[template(path = "flags.html")]
pub(crate) struct FlagList {
    shell: Shell,
    items: Vec<FlagItem>,
}

/// `GET /flag/`.
pub(crate) async fn list<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<FlagList>, PageError> {
    if phab_app_harbormaster::web::has_named_queries(
        state.database(),
        &viewer.user.phid,
        "PhabricatorFlagSearchEngine",
    )
    .await?
    {
        return Err(PageError::Fallback("saved search queries"));
    }
    show(state, viewer, query, uri).await
}

/// `GET /flag/query/<key>/`: the builtin query.
pub(crate) async fn query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<FlagList>, PageError> {
    if key != "all" {
        return Err(PageError::Fallback("a saved flag query"));
    }
    show(state, viewer, query, uri).await
}

async fn show<S: WebState>(
    state: Arc<S>,
    viewer: WebUser,
    query: Option<String>,
    uri: Uri,
) -> Result<Page<FlagList>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    if query.as_deref().is_some_and(|query| !query.is_empty()) {
        return Err(PageError::Fallback("a search query"));
    }
    let scope = Scope::new(state.as_ref(), &viewer);
    let mut params = serde_json::Map::new();
    params.insert("ownerPHIDs".to_owned(), json!([viewer.user.phid.as_str()]));
    params.insert("limit".to_owned(), json!(PAGE_SIZE + 1));
    let request = scope.request_with(params);
    let flags = phab_app_flag::query::FlagQuery
        .execute(&request)
        .await
        .map_err(method_error)?;
    let flags = flags.as_array().cloned().unwrap_or_default();
    if flags.len() > PAGE_SIZE {
        return Err(PageError::Fallback("more than a page of flags"));
    }
    let base = state.config().base_uri()?.trim_end_matches('/').to_owned();
    let text = |flag: &Value, pointer: &str| {
        flag.pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let items = flags
        .iter()
        .map(|flag| {
            let uri = text(flag, "/handle/uri");
            let uri = uri.strip_prefix(&base).map_or(uri.clone(), str::to_owned);
            let date = text(flag, "/dateCreated").parse().unwrap_or(0);
            FlagItem {
                id: text(flag, "/id"),
                object_phid: text(flag, "/objectPHID"),
                name: text(flag, "/handle/fullname"),
                uri,
                color: text(flag, "/colorName").to_lowercase(),
                color_name: text(flag, "/colorName"),
                note: text(flag, "/note"),
                date: time::format_utc(date),
            }
        })
        .collect();
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(FlagList {
        shell: Shell::for_user(state.config(), &viewer, "Flagged", current),
        items,
    }))
}
