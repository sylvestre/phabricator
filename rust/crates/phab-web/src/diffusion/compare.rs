//! `/source/<x>/compare/` (`DiffusionCompareController`): the commits on
//! `head` but not on `against`, with their graph, or the "Compare
//! Against" dialog when a ref is missing, unknown or both are the same.
//!
//! The dialog posts to PHP (Rust answers form posts with `421`), which
//! checks the refs and redirects here. Left to PHP: history Git can't
//! read, which PHP reports on the page.

use askama::Template;
use axum::response::{IntoResponse, Response};
use phab_app_diffusion::history_query::HistoryQuery;
use phab_app_diffusion::php::Key;
use phab_app_diffusion::query::Query;
use serde_json::{Value, json};

use super::history::{self, Item};
use super::{Context, Link, PAGE_SIZE, Pager};
use crate::error::PageError;
use crate::harbormaster::method_error;
use crate::response::Page;
use crate::state::WebState;
use crate::viewer::WebUser;

#[derive(Template)]
#[template(path = "diffusion_compare.html")]
pub(crate) struct ComparePage {
    shell: crate::Shell,
    head: String,
    against: String,
    reverse_href: String,
    compare_href: String,
    items: Vec<Item>,
    graph_count: usize,
    pager: Pager,
}

#[derive(Template)]
#[template(path = "diffusion_compare_dialog.html")]
pub(crate) struct CompareDialog {
    shell: crate::Shell,
    action: String,
    csrf_token: String,
    head: String,
    against: String,
    cancel_href: String,
}

/// The `head`, `against` and `offset` of the query string.
fn params(query: Option<&str>) -> Result<(String, String, i64), PageError> {
    let mut head = String::new();
    let mut against = String::new();
    let mut rest = form_urlencoded::Serializer::new(String::new());
    for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "head" => head = value.into_owned(),
            "against" => against = value.into_owned(),
            _ => {
                rest.append_pair(&key, &value);
            }
        }
    }
    let offset = super::offset(Some(&rest.finish()))?;
    Ok((head, against, offset))
}

/// `GET /source/<x>/compare/`.
pub(crate) async fn page<S: WebState>(
    state: &S,
    viewer: &WebUser,
    context: Context,
    query: Option<&str>,
) -> Result<Response, PageError> {
    let (mut head, mut against, offset) = params(query)?;
    let mut must_prompt = false;
    if head.is_empty() {
        head = context
            .commit
            .clone()
            .filter(|commit| !commit.is_empty())
            .unwrap_or_else(|| context.branch.clone());
    }
    if against.is_empty() {
        let default_branch = context.default_branch();
        if default_branch != head {
            against = default_branch;
            // A default the viewer must confirm.
            must_prompt = true;
        }
    }
    let refs: Vec<String> = [&head, &against]
        .into_iter()
        .filter(|name| !name.is_empty())
        .cloned()
        .collect();
    let mut resolved = context
        .drequest
        .resolve_refs(&context.scope.request(), &refs, &[])
        .await
        .map_err(method_error)?;
    let head_matches = resolved.shift_remove(&Key::from_text(&head));
    let against_matches = resolved.shift_remove(&Key::from_text(&against));
    let identical = head == against
        || matches!((&head_matches, &against_matches), (Some(head), Some(against))
            if head.clone().into_json() == against.clone().into_json());
    let both = head != against && head_matches.is_some() && against_matches.is_some();
    if must_prompt || !both || identical {
        let action = context.uri("compare", Link::default());
        return Ok(Page(CompareDialog {
            shell: context.shell(state, viewer, "Compare Against"),
            action,
            csrf_token: viewer.csrf_token.clone(),
            head,
            against,
            cancel_href: format!("{}browse/", context.repository.uri()),
        })
        .into_response());
    }

    let result = context
        .call(
            &Query(HistoryQuery),
            json!({
                "commit": head,
                "against": against,
                "path": context.path,
                "offset": offset,
                "limit": PAGE_SIZE + 1,
            }),
        )
        .await?;
    let mut found: Vec<(String, Option<i64>)> = result
        .get("pathChanges")
        .and_then(Value::as_array)
        .map(|changes| {
            changes
                .iter()
                .filter_map(|change| {
                    let hash = change.get("commitIdentifier")?.as_str()?.to_owned();
                    let epoch = change
                        .get("commit")
                        .and_then(|commit| commit.get("epoch"))
                        .and_then(Value::as_str)
                        .and_then(|epoch| epoch.parse().ok());
                    Some((hash, epoch))
                })
                .collect()
        })
        .unwrap_or_default();
    let base = context.compare_uri(Some(&head), Some(&against));
    let pager = Pager::slice(offset, &base, &mut found);
    let mut items = history::items(state, viewer, &context, &found).await?;
    let graph_count = if items.is_empty() {
        0
    } else {
        history::attach_graph(&mut items, super::graph::parents_of(&result), true, &pager)
    };
    let reverse_href = context.compare_uri(Some(&against), Some(&head));
    let compare_href = context.compare_uri(Some(&head), None);
    let title = context.title(&context.repository.name);
    Ok(Page(ComparePage {
        shell: context.shell(state, viewer, &title),
        head,
        against,
        reverse_href,
        compare_href,
        items,
        graph_count,
        pager,
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_refs_and_the_pager() {
        assert_eq!(
            params(Some("head=feature&against=master&offset=100")).unwrap(),
            ("feature".to_owned(), "master".to_owned(), 100)
        );
        assert!(params(Some("head=a&view=x")).is_err());
    }
}
