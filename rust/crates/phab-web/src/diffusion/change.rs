//! `/source/<x>/change/<branch>/<path>` (`DiffusionChangeController`):
//! the change of the commit which last changed a path, at the page's
//! commit (`diffusion.diffquery`: `git diff` with full context against
//! that commit's first parent).
//!
//! Left to PHP: binary and image changes, and diffs which aren't UTF-8.

use askama::Template;
use axum::response::{IntoResponse, Response};
use phab_app_diffusion::last_modified_query::LastModifiedQuery;
use phab_app_diffusion::query::Query;
use phab_diff::FileType;
use serde_json::{Value, json};

use super::browse::{Button, Header};
use super::commit::{self, EMPTY_TREE};
use super::{Context, Link, Stable};
use crate::error::PageError;
use crate::harbormaster::method_error;
use crate::response::Page;
use crate::state::WebState;
use crate::viewer::WebUser;

#[derive(Template)]
#[template(path = "diffusion_change.html")]
pub(crate) struct ChangePage {
    shell: crate::Shell,
    header: Header,
    buttons: Vec<Button>,
    changeset: String,
}

/// `GET /source/<x>/change/<blob>`.
pub(crate) async fn page<S: WebState>(
    state: &S,
    viewer: &WebUser,
    mut context: Context,
    offset: i64,
) -> Result<Response, PageError> {
    if offset != 0 {
        return Err(PageError::Fallback("a query string the pages don't read"));
    }
    // getEffectiveCommit(): the commit which last changed the path.
    let stable = context.stable().await?;
    let mut paths = serde_json::Map::new();
    paths.insert(context.path.clone(), json!(stable.hash));
    let modified = context
        .call(&Query(LastModifiedQuery), json!({"paths": paths}))
        .await?;
    let effective = modified
        .get(&context.path)
        .and_then(Value::as_str)
        .filter(|hash| !hash.is_empty())
        .map(str::to_owned)
        .ok_or(PageError::NotFound)?;

    // DiffusionGitRawDiffQuery: against the first parent, or the empty
    // tree for a root commit.
    let parents = context
        .drequest
        .copy
        .execx(
            ["log", "-n", "1", "--format=%P", &effective, "--"]
                .map(str::to_owned)
                .to_vec(),
            None,
        )
        .await
        .map_err(method_error)?;
    let against = if String::from_utf8_lossy(&parents).trim().is_empty() {
        EMPTY_TREE.to_owned()
    } else {
        format!("{effective}^")
    };
    let path_id = phab_app_diffusion::history::path_id(
        state.database(),
        &phab_app_diffusion::history::normalize_path(&context.path),
    )
    .await
    .map_err(method_error)?
    .unwrap_or(0);
    let changeset = commit::changeset(&context, &effective, &against, &context.path, path_id)
        .await?
        .ok_or(PageError::NotFound)?;
    if matches!(changeset.file_type, FileType::Binary | FileType::Image) {
        return Err(PageError::Fallback("a binary or image change"));
    }

    // updateSymbolicCommit().
    context.commit = Some(effective.clone());
    let stable = Stable {
        hash: effective,
        kind: "commit".to_owned(),
    };
    let rendered = commit::render_changesets(state, &context, vec![changeset])
        .await?
        .pop()
        .unwrap_or_default();
    let header =
        Header::new(context.path_links_html("browse"), "").tag(context.commit_tag(&stable));
    let buttons = vec![
        Button {
            text: "View History",
            icon: "fa-clock-o",
            href: context.uri("history", Link::default()),
        },
        Button {
            text: "Browse Content",
            icon: "fa-files-o",
            href: context.uri("browse", Link::default()),
        },
    ];
    let name = context
        .path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    Ok(Page(ChangePage {
        shell: context.shell(state, viewer, &context.title(&name)),
        header,
        buttons,
        changeset: rendered,
    })
    .into_response())
}
