//! The small read-only pages of a revision: why a commit closed it
//! (`/differential/revision/closedetails/<phid>/`,
//! `DifferentialRevisionCloseDetailsController`), the list of its inline
//! comments (`/differential/revision/inlines/<id>/`,
//! `DifferentialRevisionInlinesController`) and its affected path index
//! (`/differential/revision/paths/<id>/`,
//! `DifferentialRevisionAffectedPathsController`).
//!
//! PHP's revision page opens the close explanation as a dialog (an Ajax
//! request), which stays with PHP, as do transactions of objects other
//! than revisions and match data Rust doesn't read as PHP would.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use phab_app_differential::storage::{self, Revision};
use phab_app_differential::{diff_search, repository};
use phab_core::{Phid, php};
use phab_policy::application;
use serde_json::Value;

use crate::error::PageError;
use crate::harbormaster::{self, Scope};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::{diff_html, transactions};

/// `ArcanistDifferentialRevisionHash`: the hash types a commit and a diff
/// may share.
const HASH_GIT_TREE: &str = "gttr";
const HASH_GIT_COMMIT: &str = "gtcm";
const HASH_MERCURIAL_COMMIT: &str = "hgcm";

/// The current path and query of the request, for the shell.
fn current(uri: &Uri) -> &str {
    uri.path_and_query().map_or("/", |path| path.as_str())
}

/// A value as `pht()` prints it with `%s`; `None` for values Rust doesn't
/// print as PHP would.
fn php_text(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null | Value::Bool(false)) => Some(String::new()),
        Some(Value::Bool(true)) => Some("1".to_owned()),
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Number(number)) if number.is_i64() || number.is_u64() => {
            Some(number.to_string())
        }
        Some(_) => None,
    }
}

/// `getRevisionMatchExplanation()`: the paragraph of the dialog, as HTML,
/// with `link` the link to the revision. `None` for match data Rust
/// doesn't read as PHP would.
fn match_explanation(data: Option<&Value>, link: &str) -> Option<String> {
    let data = match data {
        None => return Some(no_match_data()),
        Some(value) if !php::is_truthy(value) => return Some(no_match_data()),
        Some(Value::Object(data)) => data,
        Some(_) => return None,
    };
    let text = |key: &str| php_text(data.get(key)).map(|text| diff_html::escape(&text));
    if data.get("usedURI").is_some_and(php::is_truthy) {
        return Some(format!(
            "We found a \"Differential Revision\" field with value \"{}\" in the commit message, \
             and the domain on the URI matches this install, so we linked this commit to {link}.",
            text("foundURI")?,
        ));
    }
    let mut why = Vec::new();
    if data.get("foundURI").is_some_and(php::is_truthy) {
        why.push(format!(
            "We found a \"Differential Revision\" field with value \"{}\" in the commit message, \
             but the domain on this URI did not match the configured domain for this install, \
             \"{}\", so we ignored it under the assumption that it refers to some third-party \
             revision.",
            text("foundURI")?,
            text("validDomain")?,
        ));
    } else {
        why.push(
            "We didn't find a \"Differential Revision\" field in the commit message.".to_owned(),
        );
    }
    // `switch` compares loosely: only strings and null compare as Rust does.
    let hash_type = match data.get("matchHashType") {
        None | Some(Value::Null) => None,
        Some(Value::String(kind)) => match kind.as_str() {
            HASH_GIT_TREE => Some("tree"),
            HASH_GIT_COMMIT | HASH_MERCURIAL_COMMIT => Some("commit"),
            _ => None,
        },
        Some(_) => return None,
    };
    if let Some(hash_type) = hash_type {
        why.push(format!(
            "This commit and the active diff of {link} had the same {hash_type} hash ({}) so we \
             linked this commit to {link}.",
            text("matchHashValue")?,
        ));
    }
    Some(why.join("\n"))
}

fn no_match_data() -> String {
    "This commit was made before this feature was built and thus this information is \
     unavailable."
        .to_owned()
}

#[derive(Template)]
#[template(path = "revision_close_details.html")]
pub(crate) struct CloseDetailsPage {
    shell: Shell,
    explanation_html: String,
    revision_uri: String,
}

/// `GET /differential/revision/closedetails/<phid>/`: the explanation of
/// why a commit closed the revision, as a page.
pub(crate) async fn close_details<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(phid): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<CloseDetailsPage>, PageError> {
    if transactions::is_ajax(query.as_deref()) {
        return Err(PageError::Fallback("a transaction dialog"));
    }
    let (transaction, revision) = transactions::load(state.as_ref(), &viewer, &phid).await?;
    let revision_uri = format!("/D{}", revision.id);
    let link = harbormaster::link_html(
        &format!("D{}", revision.id),
        Some(&revision_uri),
        revision.is_closed(),
    );
    let explanation_html =
        match_explanation(transaction.metadata_value("revisionMatchData"), &link)
            .ok_or(PageError::Fallback("unusual revision match data"))?;
    Ok(Page(CloseDetailsPage {
        shell: Shell::for_user(
            state.config(),
            &viewer,
            "Commit Close Explanation",
            current(&uri),
        ),
        explanation_html,
        revision_uri,
    }))
}

/// The revision with this ID, if the viewer can see it: a policy
/// exception otherwise (`executeOne()`).
async fn visible_revision<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: i64,
) -> Result<Revision, PageError> {
    viewer
        .require_application(state, application::DIFFERENTIAL)
        .await?;
    let database = state.database();
    let revision = storage::load_revisions_by_id(database, &[id])
        .await?
        .pop()
        .ok_or(PageError::NotFound)?;
    storage::visible_revisions(
        database,
        state.config(),
        &viewer.user.viewer(),
        vec![revision],
        state.now(),
    )
    .await
    .map_err(harbormaster::internal)?
    .pop()
    .ok_or(PageError::PolicyDenied)
}

/// Appends the replies to `phid`, each followed by its own replies.
fn descend(
    replies: &mut HashMap<Phid, Vec<storage::RevisionInline>>,
    phid: &Phid,
    results: &mut Vec<storage::RevisionInline>,
) {
    for reply in replies.remove(phid).unwrap_or_default() {
        let child = reply.phid.clone();
        results.push(reply);
        descend(replies, &child, results);
    }
}

/// `PHUIDiffInlineThreader::reorderAndThreadCommments()`: each comment
/// followed by its replies, depth first; replies to comments not in the
/// list stay where they are, and cycles go last.
fn thread(inlines: Vec<storage::RevisionInline>) -> Vec<storage::RevisionInline> {
    let mut inlines = inlines;
    inlines.sort_by_key(|inline| inline.id);
    let known: HashSet<Phid> = inlines.iter().map(|inline| inline.phid.clone()).collect();
    let mut replies: HashMap<Phid, Vec<storage::RevisionInline>> = HashMap::new();
    let mut order: Vec<Phid> = Vec::new();
    let mut top = Vec::new();
    for inline in &inlines {
        order.push(inline.phid.clone());
    }
    for inline in inlines {
        match inline
            .reply_to
            .clone()
            .filter(|parent| known.contains(parent))
        {
            Some(parent) => replies.entry(parent).or_default().push(inline),
            None => top.push(inline),
        }
    }
    let mut results = Vec::new();
    for inline in top {
        let phid = inline.phid.clone();
        results.push(inline);
        descend(&mut replies, &phid, &mut results);
    }
    // The cycles, in the order of the comments they reply to.
    for phid in order {
        results.extend(replies.remove(&phid).unwrap_or_default());
    }
    results
}

/// A row of the inline comment list.
pub(crate) struct InlineRow {
    /// "Current", or the diff the comment is on.
    diff: Option<i64>,
    /// The icon and what it means.
    status: (&'static str, &'static str),
    author_html: String,
    is_reply: bool,
    uri: String,
    content: String,
    age: String,
}

#[derive(Template)]
#[template(path = "revision_inlines.html")]
pub(crate) struct InlinesPage {
    shell: Shell,
    revision_uri: String,
    monogram: String,
    title: String,
    rows: Vec<InlineRow>,
}

/// `GET /differential/revision/inlines/<id>/`.
pub(crate) async fn inlines<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    uri: Uri,
) -> Result<Page<InlinesPage>, PageError> {
    let state = state.as_ref();
    let revision = visible_revision(state, &viewer, id).await?;
    let database = state.database();
    let found = storage::load_revision_inlines(database, &revision.phid).await?;
    let subjects: Vec<_> = found.iter().map(storage::RevisionInline::subject).collect();
    let visible = storage::visible_subjects(
        database,
        state.config(),
        &viewer.user.viewer(),
        &subjects,
        state.now(),
    )
    .await
    .map_err(harbormaster::internal)?;
    let shown: Vec<_> = found
        .into_iter()
        .zip(visible)
        .filter_map(|(inline, visible)| visible.then_some(inline))
        .collect();
    let shown = thread(shown);

    let scope = Scope::new(state, &viewer);
    let diffs = diff_search::revision_diffs(&scope.request(), revision.id)
        .await
        .map_err(harbormaster::method_error)?;
    let newest = diffs.iter().map(|diff| diff.id).max();
    let visible_diffs: HashSet<i64> = diffs.iter().map(|diff| diff.id).collect();
    let mut changeset_ids: Vec<i64> = shown.iter().map(|inline| inline.changeset_id).collect();
    changeset_ids.sort_unstable();
    changeset_ids.dedup();
    let changesets = storage::load_changeset_diff_ids(database, &changeset_ids).await?;
    let authors: Vec<Phid> = shown
        .iter()
        .map(|inline| inline.author_phid.clone())
        .collect();
    if !authors.iter().all(harbormaster::is_supported) {
        return Err(PageError::Fallback("a comment by an object not ported"));
    }
    let handles = harbormaster::load_handles(state, &scope, &authors).await?;

    let mut rows = Vec::with_capacity(shown.len());
    for inline in shown {
        // PHP fails on a changeset it can't load.
        let diff_id = changesets
            .get(&inline.changeset_id)
            .copied()
            .filter(|diff| visible_diffs.contains(diff))
            .ok_or(PageError::Fallback(
                "an inline comment on a changeset the viewer can't load",
            ))?;
        let status = if inline.fixed_state.as_deref() == Some("done") {
            ("fa-check green", "Done")
        } else if inline.reply_to.is_some()
            && Some(&inline.author_phid) == revision.author_phid.as_ref()
        {
            ("fa-commenting-o blue", "Replied to by the author")
        } else {
            ("fa-circle-o grey", "Not done")
        };
        rows.push(InlineRow {
            diff: (Some(diff_id) != newest).then_some(diff_id),
            status,
            author_html: harbormaster::handle(&handles, &inline.author_phid).link_html(),
            is_reply: inline.reply_to.is_some(),
            uri: format!("/D{}#inline-{}", revision.id, inline.id),
            content: inline.content,
            age: crate::system::relative_time(state.now() - inline.date_modified),
        });
    }
    let monogram = format!("D{}", revision.id);
    Ok(Page(InlinesPage {
        shell: Shell::for_user(
            state.config(),
            &viewer,
            &format!("{monogram} {} · Inlines", revision.title),
            current(&uri),
        ),
        revision_uri: format!("/D{}", revision.id),
        monogram,
        title: revision.title,
        rows,
    }))
}

/// A row of the affected path index.
pub(crate) struct PathRow {
    repository_id: Option<i64>,
    repository_html: Option<String>,
    path_id: i64,
    path: Option<String>,
}

#[derive(Template)]
#[template(path = "revision_paths.html")]
pub(crate) struct PathsPage {
    shell: Shell,
    revision_uri: String,
    monogram: String,
    rows: Vec<PathRow>,
}

/// `GET /differential/revision/paths/<id>/`.
pub(crate) async fn paths<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    uri: Uri,
) -> Result<Page<PathsPage>, PageError> {
    let state = state.as_ref();
    let revision = visible_revision(state, &viewer, id).await?;
    let database = state.database();
    let affected = storage::load_affected_paths(database, revision.id).await?;
    let mut repository_ids: Vec<i64> = affected.iter().filter_map(|(id, _)| *id).collect();
    repository_ids.sort_unstable();
    repository_ids.dedup();
    let repositories = repository::visible(
        database,
        state.config(),
        &viewer.user.viewer(),
        repository::load_by_ids(database, &repository_ids).await?,
        state.now(),
    )
    .await
    .map_err(harbormaster::internal)?;
    let scope = Scope::new(state, &viewer);
    let phids: Vec<Phid> = repositories
        .iter()
        .map(|repository| repository.phid.clone())
        .collect();
    let handles = harbormaster::load_handles(state, &scope, &phids).await?;
    let repositories: HashMap<i64, Phid> = repositories
        .into_iter()
        .map(|repository| (repository.id, repository.phid))
        .collect();
    let mut path_ids: Vec<i64> = affected.iter().map(|(_, path)| *path).collect();
    path_ids.sort_unstable();
    path_ids.dedup();
    let names = storage::load_path_names(database, &path_ids).await?;
    let mut rows: Vec<PathRow> = affected
        .into_iter()
        .map(|(repository_id, path_id)| PathRow {
            repository_id,
            repository_html: repository_id
                .and_then(|id| repositories.get(&id))
                .map(|phid| harbormaster::handle(&handles, phid).link_html()),
            path_id,
            path: names.get(&path_id).cloned(),
        })
        .collect();
    // isort() by the path; paths begin with "/", so none compares as a
    // number.
    rows.sort_by(|a, b| {
        a.path
            .as_deref()
            .unwrap_or("")
            .cmp(b.path.as_deref().unwrap_or(""))
    });
    let monogram = format!("D{}", revision.id);
    Ok(Page(PathsPage {
        shell: Shell::for_user(
            state.config(),
            &viewer,
            &format!("{monogram} · Affected Path Index"),
            current(&uri),
        ),
        revision_uri: format!("/D{}", revision.id),
        monogram,
        rows,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const LINK: &str = "<a href=\"/D1\">D1</a>";

    #[test]
    fn explains_matches_like_php() {
        assert_eq!(
            match_explanation(None, LINK).unwrap(),
            "This commit was made before this feature was built and thus this information is \
             unavailable."
        );
        assert_eq!(
            match_explanation(Some(&json!([])), LINK).unwrap(),
            match_explanation(None, LINK).unwrap()
        );
        assert_eq!(
            match_explanation(
                Some(&json!({"usedURI": true, "foundURI": "https://x/D1<"})),
                LINK
            )
            .unwrap(),
            "We found a \"Differential Revision\" field with value \"https://x/D1&lt;\" in the \
             commit message, and the domain on the URI matches this install, so we linked this \
             commit to <a href=\"/D1\">D1</a>."
        );
        let hashed = match_explanation(
            Some(&json!({
                "usedURI": false, "foundURI": null, "matchHashType": "gttr",
                "matchHashValue": "abc",
            })),
            LINK,
        )
        .unwrap();
        assert_eq!(
            hashed,
            "We didn't find a \"Differential Revision\" field in the commit message.\nThis \
             commit and the active diff of <a href=\"/D1\">D1</a> had the same tree hash (abc) so \
             we linked this commit to <a href=\"/D1\">D1</a>."
        );
        assert!(match_explanation(Some(&json!({"matchHashType": true})), LINK).is_none());
        assert!(match_explanation(Some(&json!("x")), LINK).is_none());
    }

    fn inline(id: i64, reply_to: Option<&str>) -> storage::RevisionInline {
        storage::RevisionInline {
            id,
            phid: Phid::new(format!("PHID-XCMT-{id}")),
            author_phid: Phid::new("PHID-USER-a"),
            view_policy: "users".to_owned(),
            edit_policy: "users".to_owned(),
            content: String::new(),
            changeset_id: 1,
            fixed_state: None,
            reply_to: reply_to.map(Phid::new),
            date_modified: 0,
        }
    }

    #[test]
    fn threads_like_php() {
        let threaded = thread(vec![
            inline(4, Some("PHID-XCMT-1")),
            inline(1, None),
            inline(2, None),
            inline(3, Some("PHID-XCMT-2")),
            inline(5, Some("PHID-XCMT-4")),
            inline(6, Some("PHID-XCMT-gone")),
            inline(7, Some("PHID-XCMT-8")),
            inline(8, Some("PHID-XCMT-7")),
        ]);
        let ids: Vec<i64> = threaded.iter().map(|inline| inline.id).collect();
        assert_eq!(ids, [1, 4, 5, 2, 3, 6, 8, 7]);
    }
}
