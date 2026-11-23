//! `/herald/` and `/herald/transcript/`: the builtin queries of
//! `HeraldRuleSearchEngine` (Authored, Active, All) and
//! `HeraldTranscriptSearchEngine` (All Transcripts). Other queries,
//! filters and saved queries are PHP's.

use std::collections::HashSet;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use phab_app_harbormaster::web as harbormaster_web;
use phab_app_herald::web::{self as herald, RuleFilter, RuleRow};
use phab_core::Phid;

use super::{APPLICATION, adapter_name, load_handles};
use crate::error::PageError;
use crate::harbormaster::{self, Scope};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorApplicationSearchEngine` shows this many results a page.
const PAGE_SIZE: usize = 100;

/// A rule in the list (`HeraldRuleListView`).
pub(crate) struct RuleItem {
    id: i64,
    name: String,
    kind: &'static str,
    icon: &'static str,
    author_html: Option<String>,
    disabled: bool,
    author_inactive: bool,
    affects: &'static str,
}

#[derive(Template)]
#[template(path = "herald_rules.html")]
pub(crate) struct RuleList {
    shell: Shell,
    query: &'static str,
    title: &'static str,
    items: Vec<RuleItem>,
    next: Option<i64>,
}

/// A transcript in the list.
pub(crate) struct TranscriptItem {
    id: i64,
    dry_run: bool,
    object_html: String,
    duration: String,
    date: String,
}

#[derive(Template)]
#[template(path = "herald_transcripts.html")]
pub(crate) struct TranscriptList {
    shell: Shell,
    items: Vec<TranscriptItem>,
    next: Option<i64>,
}

/// The `after` cursor of a list's query string; any other parameter is a
/// query PHP builds.
pub(crate) fn cursor(query: Option<&str>) -> Result<Option<i64>, PageError> {
    let mut after = None;
    for (key, value) in form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
        match key.as_ref() {
            "after" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                after = Some(
                    value
                        .parse()
                        .map_err(|_| PageError::Fallback("an unusual cursor"))?,
                );
            }
            _ => return Err(PageError::Fallback("a search query")),
        }
    }
    Ok(after)
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

/// The builtin rule queries: their name and filter.
fn builtin(key: &str) -> Option<&'static str> {
    Some(match key {
        "authored" => "Authored",
        "active" => "Active",
        "all" => "All",
        _ => return None,
    })
}

async fn rule_list<S: WebState>(
    state: &S,
    viewer: &WebUser,
    key: &'static str,
    query: Option<&str>,
    uri: &Uri,
) -> Result<Page<RuleList>, PageError> {
    let after = cursor(query)?;
    let database = state.database();
    let viewer_phid = [viewer.user.phid.clone()];
    let filter = match key {
        "authored" => RuleFilter {
            authors: Some(&viewer_phid),
            disabled: Some(false),
            ..RuleFilter::default()
        },
        "active" => RuleFilter {
            disabled: Some(false),
            ..RuleFilter::default()
        },
        _ => RuleFilter::default(),
    };
    let scope = Scope::new(state, viewer);
    let rules: Vec<RuleRow> = herald::load_rules(database, &filter)
        .await?
        .into_iter()
        .filter(|rule| after.is_none_or(|after| rule.id < after))
        .collect();
    let rules = super::rule::visible_rules(state, &scope, rules).await?;

    // `validateRuleAuthors()`: personal rules need an active author.
    let authors: Vec<Phid> = rules
        .iter()
        .filter(|rule| rule.rule_type == "personal")
        .map(|rule| rule.author_phid.clone())
        .collect();
    let active: HashSet<Phid> = phab_auth::user::load_users_by_phid(database, &authors)
        .await?
        .into_iter()
        .filter(|user| user.is_activated(state.config()))
        .map(|user| user.phid)
        .collect();
    let valid = |rule: &RuleRow| rule.rule_type != "personal" || active.contains(&rule.author_phid);
    let mut rules: Vec<RuleRow> = rules
        .into_iter()
        .filter(|rule| key != "active" || valid(rule))
        .collect();
    let next = (rules.len() > PAGE_SIZE).then(|| rules[PAGE_SIZE - 1].id);
    rules.truncate(PAGE_SIZE);

    let shown_authors: Vec<Phid> = rules
        .iter()
        .filter(|rule| rule.rule_type == "personal")
        .map(|rule| rule.author_phid.clone())
        .collect();
    let handles = load_handles(state, &scope, &shown_authors).await?;
    let items = rules
        .iter()
        .map(|rule| {
            let (kind, icon) = match rule.rule_type.as_str() {
                "personal" => ("Personal Rule", "fa-user"),
                "object" => ("Object Rule", "fa-briefcase"),
                _ => ("Global Rule", "fa-globe"),
            };
            RuleItem {
                id: rule.id,
                name: rule.name.clone(),
                kind,
                icon,
                author_html: (rule.rule_type == "personal")
                    .then(|| harbormaster::handle(&handles, &rule.author_phid).link_html()),
                disabled: rule.is_disabled,
                author_inactive: !valid(rule),
                affects: adapter_name(&rule.content_type).unwrap_or(""),
            }
        })
        .collect();
    let title = builtin(key).unwrap_or("All");
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(RuleList {
        shell: Shell::for_user(state.config(), viewer, "Herald", current),
        query: key,
        title,
        items,
        next,
    }))
}

/// `GET /herald/`: the viewer's default query, the first builtin one
/// ("Authored") unless they saved or reordered queries.
pub(crate) async fn rules<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<RuleList>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    if harbormaster_web::has_named_queries(
        state.database(),
        &viewer.user.phid,
        "HeraldRuleSearchEngine",
    )
    .await?
    {
        return Err(PageError::Fallback("saved search queries"));
    }
    rule_list(state.as_ref(), &viewer, "authored", query.as_deref(), &uri).await
}

/// `GET /herald/query/{key}/`: a builtin query.
pub(crate) async fn rules_query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<RuleList>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let key = match key.as_str() {
        "authored" => "authored",
        "active" => "active",
        "all" => "all",
        _ => return Err(PageError::Fallback("a saved search query")),
    };
    rule_list(state.as_ref(), &viewer, key, query.as_deref(), &uri).await
}

/// `GET /herald/transcript/` and `/herald/transcript/query/all/`.
pub(crate) async fn transcripts<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<TranscriptList>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let path = uri.path();
    if path != "/herald/transcript/" && path != "/herald/transcript/query/all/" {
        return Err(PageError::Fallback("a saved search query"));
    }
    let after = cursor(query.as_deref())?;
    if path == "/herald/transcript/"
        && harbormaster_web::has_named_queries(
            state.database(),
            &viewer.user.phid,
            "HeraldTranscriptSearchEngine",
        )
        .await?
    {
        return Err(PageError::Fallback("saved search queries"));
    }
    let state = state.as_ref();
    let scope = Scope::new(state, &viewer);
    // `PhabricatorCursorPagedPolicyAwareQuery`: pages of rows until enough
    // of them are about objects the viewer can see.
    let mut shown = Vec::new();
    let mut handles = std::collections::HashMap::new();
    let mut before = after;
    loop {
        let rows = herald::transcripts(state.database(), before, PAGE_SIZE + 1).await?;
        let exhausted = rows.len() <= PAGE_SIZE;
        before = rows.last().map(|row| row.id);
        if rows.iter().any(|row| row.object_phid.is_empty()) {
            return Err(PageError::Fallback("a transcript without object"));
        }
        let objects: Vec<Phid> = rows
            .iter()
            .map(|row| Phid::new(row.object_phid.as_str()))
            .collect();
        let found = load_handles(state, &scope, &objects).await?;
        for row in rows {
            let object = Phid::new(row.object_phid.as_str());
            if found
                .get(&object)
                .is_some_and(|handle| handle.uri.is_some())
            {
                shown.push(row);
            }
        }
        handles.extend(found);
        if shown.len() > PAGE_SIZE || exhausted {
            break;
        }
    }
    let next = (shown.len() > PAGE_SIZE).then(|| shown[PAGE_SIZE - 1].id);
    shown.truncate(PAGE_SIZE);
    let items = shown
        .iter()
        .map(|row| {
            #[allow(clippy::cast_possible_truncation)]
            let milliseconds = (1000.0 * row.duration) as i64;
            TranscriptItem {
                id: row.id,
                dry_run: row.dry_run,
                object_html: harbormaster::handle(&handles, &Phid::new(row.object_phid.as_str()))
                    .link_html(),
                duration: format!("{} ms", thousands(milliseconds)),
                date: crate::time::format_utc(row.time),
            }
        })
        .collect();
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(TranscriptList {
        shell: Shell::for_user(state.config(), &viewer, "Herald Transcripts", current),
        items,
        next,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_grouped() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(-1_234), "-1,234");
    }

    #[test]
    fn only_the_cursor_is_ported() {
        assert_eq!(cursor(Some("after=5")).unwrap(), Some(5));
        assert!(cursor(Some("ruleTypes[0]=global")).is_err());
    }
}
