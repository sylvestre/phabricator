//! The review queue, on `/` and `/differential/`: the open revisions the
//! viewer is responsible for, grouped by required action
//! (`DifferentialRevisionSearchEngine`'s "active" query with a bucket, W5).

use std::collections::HashSet;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Query, State};
use axum::http::Uri;
use phab_app_differential::buckets::{self, Bucket};
use phab_app_differential::storage::{self, Revision};
use phab_app_project as project;
use phab_core::Phid;
use phab_policy::application;

use crate::error::PageError;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::time;
use crate::viewer::WebUser;

/// The buckets the page offers.
const BUCKETS: [Bucket; 3] = [
    Bucket::RequiredAction,
    Bucket::WithNeedsChanges,
    Bucket::NotAccepted,
];

/// A revision in a group.
pub(crate) struct Item {
    pub(crate) monogram: String,
    pub(crate) title: String,
    pub(crate) author: String,
    pub(crate) status: &'static str,
    pub(crate) updated: String,
}

/// A group of the queue.
pub(crate) struct GroupView {
    pub(crate) name: &'static str,
    pub(crate) no_data: Option<&'static str>,
    pub(crate) items: Vec<Item>,
}

#[derive(Template)]
#[template(path = "queue.html")]
pub(crate) struct QueuePage {
    shell: Shell,
    groups: Vec<GroupView>,
    bucket: Bucket,
    buckets: [Bucket; 3],
}

/// The query string of the page.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct Params {
    pub(crate) bucket: Option<String>,
}

/// `DifferentialResponsibleDatasource::expandResponsibleUsers()`: the user
/// and the projects they are a member of which the viewer can see.
async fn responsible<S: WebState>(state: &S, viewer: &WebUser) -> Result<HashSet<Phid>, PageError> {
    let projects =
        project::storage::load_member_projects(state.database(), &viewer.user.phid).await?;
    let (projects, _) = project::policy::visible_projects(
        state.database(),
        state.config(),
        &viewer.user.viewer(),
        projects,
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    let mut phids: HashSet<Phid> = projects.into_iter().map(|project| project.phid).collect();
    phids.insert(viewer.user.phid.clone());
    Ok(phids)
}

/// `DifferentialRevisionStatus` names, shortened like the list views.
fn status_name(status: &str) -> &'static str {
    match status {
        "needs-review" => "Needs Review",
        "needs-revision" => "Needs Revision",
        "changes-planned" => "Changes Planned",
        "accepted" => "Accepted",
        "draft" => "Draft",
        _ => "Closed",
    }
}

pub(crate) async fn items<S: WebState>(
    state: &S,
    revisions: &[Revision],
) -> Result<Vec<Item>, PageError> {
    let authors: Vec<Phid> = revisions
        .iter()
        .filter_map(|revision| revision.author_phid.clone())
        .collect();
    let users = phab_auth::user::load_users_by_phid(state.database(), &authors).await?;
    Ok(revisions
        .iter()
        .map(|revision| Item {
            monogram: format!("D{}", revision.id),
            title: revision.title.clone(),
            author: revision
                .author_phid
                .as_ref()
                .and_then(|author| users.iter().find(|user| user.phid == *author))
                .map_or_else(String::new, |user| user.user_name.clone()),
            status: status_name(&revision.status),
            updated: time::format_utc(revision.date_modified),
        })
        .collect())
}

/// The "active" query of `DifferentialRevisionSearchEngine` for the
/// viewer, grouped by `bucket`.
pub(crate) async fn active_groups<S: WebState>(
    state: &S,
    viewer: &WebUser,
    bucket: Bucket,
) -> Result<Vec<GroupView>, PageError> {
    let phids: Vec<Phid> = responsible(state, viewer).await?.into_iter().collect();
    let revisions = storage::load_responsible_revisions(state.database(), &phids).await?;
    let revisions = storage::visible_revisions(
        state.database(),
        state.config(),
        &viewer.user.viewer(),
        revisions,
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    // `differential.sticky-accept` defaults to true.
    let sticky_accept = state
        .config()
        .value("differential.sticky-accept")
        .is_none_or(phab_core::php::is_truthy);
    let mut groups = Vec::new();
    for group in buckets::group(revisions, &phids, &viewer.user.phid, bucket, sticky_accept) {
        groups.push(GroupView {
            name: group.name,
            no_data: group.no_data,
            items: items(state, &group.revisions).await?,
        });
    }
    Ok(groups)
}

/// `GET /` and `GET /differential/`.
pub(crate) async fn show<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Query(params): Query<Params>,
    uri: Uri,
) -> Result<Page<QueuePage>, PageError> {
    viewer
        .require_application(
            &*state,
            if uri.path() == "/" {
                application::HOME
            } else {
                application::DIFFERENTIAL
            },
        )
        .await?;
    let bucket = params
        .bucket
        .as_deref()
        .and_then(Bucket::from_key)
        .unwrap_or(Bucket::RequiredAction);
    let groups = active_groups(state.as_ref(), &viewer, bucket).await?;
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(QueuePage {
        shell: Shell::for_user(state.config(), &viewer, "Active Revisions", current),
        groups,
        bucket,
        buckets: BUCKETS,
    }))
}
