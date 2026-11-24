//! `/p/<username>/`: a user's profile (`PhabricatorPeopleProfileViewController`).

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::Uri;
use phab_app_differential::storage::{self, Revision};
use phab_app_people::profile;
use phab_app_project as project;
use phab_core::Phid;
use phab_policy::{Viewer, application};
use phab_remarkup::{Markup, Mode};

use crate::error::PageError;
use crate::feed::{self, StoryItem};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::{remarkup, time};

/// How many projects the side column lists.
const PROJECT_LIMIT: usize = 5;
/// `buildPeopleFeed()` shows this many stories.
const ACTIVITY_LIMIT: usize = 100;
/// `PhabricatorUserReviewQueueField` looks at this many revisions.
const REVIEW_QUEUE_LIMIT: usize = 100;

/// A project in the list.
pub(crate) struct ProjectItem {
    name: String,
    uri: String,
}

#[derive(Template)]
#[template(path = "profile.html")]
pub(crate) struct Profile {
    shell: Shell,
    user_name: String,
    real_name: String,
    user_phid: String,
    user_id: i64,
    title: String,
    icon: Option<&'static str>,
    roles: Vec<&'static str>,
    since: Option<(String, String)>,
    review_queue: usize,
    blurb: Option<Markup>,
    projects: Vec<ProjectItem>,
    project_count: usize,
    stories: Vec<StoryItem>,
    skipped_stories: usize,
}

impl Profile {
    fn blurb_html(&self) -> &str {
        self.blurb.as_ref().map_or("", |blurb| {
            std::str::from_utf8(blurb.as_bytes()).unwrap_or("")
        })
    }
}

/// `PhabricatorUserReviewQueueField` (a Mozilla field): the revisions
/// waiting on this reviewer, among the first open ones they review.
pub(crate) fn review_queue_length(revisions: &[Revision], reviewer: &Phid) -> usize {
    revisions
        .iter()
        .take(REVIEW_QUEUE_LIMIT)
        // Drafts, planned changes and so on are not waiting.
        .filter(|revision| revision.status == "needs-review")
        .flat_map(|revision| &revision.reviewers)
        .filter(|entry| entry.phid == *reviewer && entry.status != "accepted")
        .count()
}

async fn projects<S: WebState>(
    state: &S,
    viewer: &Viewer,
    user: &Phid,
) -> Result<Vec<ProjectItem>, PageError> {
    let mut projects: Vec<project::Project> =
        project::storage::load_member_projects(state.database(), user)
            .await?
            .into_iter()
            .filter(|project| !project.is_archived())
            .collect();
    projects.sort_by_cached_key(|project| project.name.to_lowercase());
    let (visible, graph) = project::policy::visible_projects(
        state.database(),
        state.config(),
        viewer,
        projects,
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    Ok(visible
        .iter()
        .map(|project| ProjectItem {
            name: project.display_name(graph.parent(project)),
            uri: project.uri(),
        })
        .collect())
}

/// `GET /p/{username}/`.
pub(crate) async fn show<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(username): Path<String>,
    uri: Uri,
) -> Result<Page<Profile>, PageError> {
    viewer
        .require_application(&*state, application::PEOPLE)
        .await?;
    let database = state.database();
    let user = phab_auth::user::load_users_by_username(database, &[username.as_str()])
        .await?
        .into_iter()
        .next()
        .ok_or(PageError::NotFound)?;
    let as_viewer = viewer.user.viewer();

    let user_profile = profile::load_profile(database, &user.phid).await?;
    let since = profile::date_created(database, &user.phid)
        .await?
        .map(|created| {
            (
                time::format_utc(created),
                profile::relative_time(state.now() - created, 2),
            )
        });
    let reviewed = storage::load_open_revisions_reviewed_by(database, &user.phid).await?;
    let reviewed =
        storage::visible_revisions(database, state.config(), &as_viewer, reviewed, state.now())
            .await
            .map_err(|error| PageError::Internal(Box::new(error)))?;
    let blurb = if user_profile.blurb.is_empty() {
        None
    } else {
        Some(remarkup::render(state.as_ref(), &as_viewer, &user_profile.blurb, Mode::Html).await?)
    };
    // `buildPeopleFeed()`: the stories referencing the user.
    let (activity, activity_handles) = feed::load(
        state.as_ref(),
        &viewer,
        phab_app_feed::listing::Query {
            cursor: phab_app_feed::listing::Cursor::Newest,
            limit: ACTIVITY_LIMIT,
            references: Some(std::slice::from_ref(&user.phid)),
        },
    )
    .await?;
    let mut projects = projects(state.as_ref(), &as_viewer, &user.phid).await?;
    let project_count = projects.len();
    projects.truncate(PROJECT_LIMIT);

    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(Profile {
        shell: Shell::for_user(state.config(), &viewer, &user.user_name, current),
        title: user_profile.display_title().to_owned(),
        icon: user_profile.icon_class(),
        roles: profile::roles(&user),
        since,
        review_queue: review_queue_length(&reviewed, &user.phid),
        blurb,
        projects,
        project_count,
        stories: feed::items(&activity, &activity_handles),
        skipped_stories: activity.skipped,
        user_phid: user.phid.as_str().to_owned(),
        user_id: user.id,
        user_name: user.user_name,
        real_name: user.real_name,
    }))
}

/// `buildRevisionsView()` shows this many revisions.
const REVISION_LIMIT: usize = 100;
/// How many rounds of rows the revisions may take before giving up, like
/// the overheating of policy-aware queries.
const MAX_ROUNDS: usize = 10;

#[derive(Template)]
#[template(path = "user_revisions.html")]
pub(crate) struct UserRevisions {
    shell: Shell,
    user_name: String,
    real_name: String,
    revisions: Vec<crate::queue::Item>,
}

/// The first [`REVISION_LIMIT`] revisions `author` wrote which `viewer` can
/// see, most recently updated first (`setLimit(100)->execute()`).
async fn authored_revisions<S: WebState>(
    state: &S,
    viewer: &Viewer,
    author: &Phid,
) -> Result<Vec<Revision>, PageError> {
    let database = state.database();
    let mut memory = phab_policy::QueryMemory::default();
    let mut visible = Vec::new();
    let mut after = None;
    for _ in 0..MAX_ROUNDS {
        let rows =
            storage::load_revisions_by_author(database, author, after, REVISION_LIMIT).await?;
        let exhausted = rows.len() < REVISION_LIMIT;
        after = rows
            .last()
            .map(|revision| (revision.date_modified, revision.id));
        visible.extend(
            storage::visible_revisions_in_query(
                database,
                state.config(),
                viewer,
                rows,
                state.now(),
                &mut memory,
            )
            .await
            .map_err(|error| PageError::Internal(Box::new(error)))?,
        );
        if visible.len() >= REVISION_LIMIT || exhausted {
            visible.truncate(REVISION_LIMIT);
            return Ok(visible);
        }
    }
    // PHP's query overheats and fails the page.
    Err(PageError::Fallback(
        "revisions of a user the viewer mostly can't see",
    ))
}

/// `GET /people/revisions/{id}/` (`PhabricatorPeopleProfileRevisionsController`).
pub(crate) async fn revisions<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    uri: Uri,
) -> Result<Page<UserRevisions>, PageError> {
    viewer
        .require_application(&*state, application::PEOPLE)
        .await?;
    let user = phab_auth::user::load_user_by_id(state.database(), id)
        .await?
        .ok_or(PageError::NotFound)?;
    // No route without Differential.
    viewer
        .require_application(&*state, application::DIFFERENTIAL)
        .await?;
    let revisions = authored_revisions(state.as_ref(), &viewer.user.viewer(), &user.phid).await?;
    let revisions = crate::queue::items(state.as_ref(), &revisions).await?;
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(Page(UserRevisions {
        shell: Shell::for_user(
            state.config(),
            &viewer,
            &format!("Recent Revisions · {}", user.user_name),
            current,
        ),
        user_name: user.user_name,
        real_name: user.real_name,
        revisions,
    }))
}

#[cfg(test)]
mod tests {
    use phab_app_differential::storage::Reviewer;

    use super::*;

    fn revision(status: &str, reviewers: &[(&str, &str)]) -> Revision {
        Revision {
            id: 1,
            phid: Phid::new("PHID-DREV-1"),
            title: String::new(),
            status: status.to_owned(),
            author_phid: None,
            view_policy: "users".to_owned(),
            edit_policy: "users".to_owned(),
            repository_phid: None,
            active_diff_phid: None,
            date_modified: 0,
            reviewers: reviewers
                .iter()
                .map(|(phid, status)| Reviewer::new(Phid::new(*phid), status))
                .collect(),
        }
    }

    #[test]
    fn review_queue_counts_revisions_waiting_on_the_reviewer() {
        let alice = Phid::new("PHID-USER-alice");
        let revisions = [
            revision("needs-review", &[("PHID-USER-alice", "added")]),
            revision("needs-review", &[("PHID-USER-alice", "accepted")]),
            revision("changes-planned", &[("PHID-USER-alice", "added")]),
            revision("needs-review", &[("PHID-USER-bob", "added")]),
        ];
        assert_eq!(review_queue_length(&revisions, &alice), 1);
    }
}
