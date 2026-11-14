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
fn review_queue_length(revisions: &[Revision], reviewer: &Phid) -> usize {
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
    let activity = feed::load(
        state.as_ref(),
        &as_viewer,
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
        stories: feed::items(&activity),
        skipped_stories: activity.skipped,
        user_phid: user.phid.as_str().to_owned(),
        user_name: user.user_name,
        real_name: user.real_name,
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
