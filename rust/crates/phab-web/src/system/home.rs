//! `/` (`PhabricatorHomeMenuItemController`): the home menu
//! (`PhabricatorHomeProfileMenuEngine`) and its default item, the builtin
//! home page (`PHUIHomeView`): the viewer's active revisions, the active
//! repositories and the recent activity.
//!
//! Menus customized by administrators or the viewer (stored items, which
//! may pin dashboards as the home page) go to PHP, as do the `/home/menu/`
//! routes.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::Uri;
use phab_app_differential::buckets::Bucket;
use phab_app_differential::repository;
use phab_app_feed::listing::{Cursor, Query as FeedQuery};
use phab_db::{DbError, rows};
use phab_policy::{ApplicationPolicies, application};

use super::{Body, Item, Link, List, PageBuilder, Section, SystemPage, catalog};
use crate::diff_html::escape;
use crate::error::PageError;
use crate::queue::{self, Params};
use crate::response::Page;
use crate::state::WebState;
use crate::time;
use crate::viewer::WebUser;

/// The home menu's profile object.
const HOME_PHID: &str = "PHID-APPS-PhabricatorHomeApplication";

/// Whether anyone stored items of the home menu.
async fn has_stored_items<S: WebState>(state: &S) -> Result<bool, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT 1 FROM {} WHERE profilePHID = ? LIMIT 1",
        database.table("search", "search_profilepanelconfiguration")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(HOME_PHID)
        .fetch_optional(database.pool())
        .await
        .map_err(DbError::from)?
        .is_some())
}

/// The builtin menu (`getBuiltinProfileItems()`): Home, the "Favorites"
/// label, the applications pinned by default the viewer can use, "More
/// Applications", and "Edit Menu".
async fn menu<S: WebState>(state: &S, viewer: &WebUser) -> Result<Vec<Link>, PageError> {
    let policies = ApplicationPolicies::from_config(state.config());
    // `withInstalled(true)->withUnlisted(false)->withLaunchable(true)`, in
    // `getAllApplications()` order.
    let candidates: Vec<&catalog::Application> = catalog::applications()
        .iter()
        .filter(|application| {
            policies.is_installed(&application.class)
                && !application.unlisted
                && application.launchable
        })
        .collect();
    let mut classes: Vec<&str> = candidates.iter().map(|app| app.class.as_str()).collect();
    classes.push("PhabricatorDiffusionApplication");
    let visible = viewer.can_see_applications(state, &classes).await?;
    let can_use_diffusion = visible.last().copied().unwrap_or(false);
    let mut links = vec![Link::new("Home", "/").icon("fa-home").inactive(true)];
    for (application, visible) in candidates.iter().zip(visible) {
        let pinned = if application.class == "PhabricatorAuditApplication" {
            can_use_diffusion
        } else {
            application.is_pinned_by_default(viewer.user.is_admin)
        };
        if !visible || !pinned {
            continue;
        }
        let Some(uri) = application.base_uri.clone() else {
            continue;
        };
        links.push(Link {
            name: application.name.clone(),
            uri,
            icon: None,
            inactive: false,
        });
    }
    links.push(Link::new("More Applications", "/applications/").icon("fa-ellipsis-h"));
    links.push(Link::new("Edit Menu", "/home/menu/configure/").icon("fa-pencil"));
    Ok(links)
}

/// `PhabricatorRepositoryType::getNameForRepositoryType()`.
fn vcs_name(vcs: &str) -> &str {
    match vcs {
        "git" => "Git",
        "hg" => "Mercurial",
        "svn" => "Subversion",
        other => other,
    }
}

/// The "Active Repositories" panel: tracked repositories the viewer can
/// see, by most recent commit, five of them.
async fn repositories<S: WebState>(state: &S, viewer: &WebUser) -> Result<Vec<Item>, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT r.id, s.size, s.epoch, c.commitIdentifier, c.summary FROM {} r \
         LEFT JOIN {} s ON s.repositoryID = r.id LEFT JOIN {} c ON c.id = s.lastCommitID \
         ORDER BY s.epoch IS NULL, s.epoch DESC, r.id DESC",
        database.table("repository", "repository"),
        database.table("repository", "repository_summary"),
        database.table("repository", "repository_commit"),
    );
    let found = phab_db::sqlx::query(&sql)
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?;
    let ids: Vec<i64> = found
        .iter()
        .map(|row| rows::int(row, "id"))
        .collect::<Result<_, _>>()?;
    let loaded = repository::load_by_ids(database, &ids).await?;
    let visible = repository::visible(
        database,
        state.config(),
        &viewer.user.viewer(),
        loaded,
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    let mut items = Vec::new();
    for row in &found {
        if items.len() == 5 {
            break;
        }
        let id = rows::int(row, "id")?;
        let Some(repository) = visible.iter().find(|repository| repository.id == id) else {
            continue;
        };
        if !repository.is_tracked() {
            continue;
        }
        let mut attributes = vec![escape(vcs_name(&repository.vcs))];
        if let (Some(identifier), Some(epoch)) = (
            rows::opt_text(row, "commitIdentifier")?,
            rows::opt_int(row, "epoch")?,
        ) {
            let local = if repository.vcs == "svn" {
                identifier.clone()
            } else {
                identifier.chars().take(12).collect()
            };
            attributes.insert(
                0,
                format!(
                    "<a href=\"{}\">{}: {}</a> · {}",
                    escape(&repository.commit_uri(&identifier)),
                    escape(&local),
                    escape(&rows::opt_text(row, "summary")?.unwrap_or_default()),
                    escape(&time::format_utc(epoch))
                ),
            );
        }
        if let Some(size) = rows::opt_int(row, "size")?.filter(|size| *size > 0) {
            attributes.push(format!(
                "<a href=\"{}history/\">{} Commit(s)</a>",
                escape(&repository.uri()),
                super::thousands(size)
            ));
        }
        items.push(Item {
            name: format!("{} {}", repository.monogram(), repository.name),
            uri: Some(repository.uri()),
            attributes_html: attributes,
            ..Item::default()
        });
    }
    Ok(items)
}

/// `GET /`.
pub(crate) async fn show<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Query(params): Query<Params>,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    let state = state.as_ref();
    viewer.require_application(state, application::HOME).await?;
    if uri.query().is_some_and(|query| !query.is_empty()) && params.bucket.is_none() {
        return Err(PageError::Fallback("a query string"));
    }
    if has_stored_items(state).await? {
        return Err(PageError::Fallback("a customized home menu"));
    }
    let menu = menu(state, &viewer).await?;
    let mut page = PageBuilder::new("Home").icon("fa-home");

    // `PHUIHomeView`.
    let can_use = viewer
        .can_see_applications(
            state,
            &[application::DIFFERENTIAL, "PhabricatorDiffusionApplication"],
        )
        .await?;
    if can_use[0] {
        let bucket = params
            .bucket
            .as_deref()
            .and_then(Bucket::from_key)
            .unwrap_or(Bucket::RequiredAction);
        for group in queue::active_groups(state, &viewer, bucket).await? {
            let items = group
                .items
                .iter()
                .map(|item| Item {
                    name: format!("{}: {}", item.monogram, item.title),
                    uri: Some(format!("/{}", item.monogram)),
                    tags: vec![item.status.to_owned()],
                    attributes_html: vec![format!(
                        "{} · {}",
                        escape(&item.author),
                        escape(&item.updated)
                    )],
                    ..Item::default()
                })
                .collect();
            page = page.section(Section::new(
                format!("Active Revisions: {}", group.name),
                Body::List(List {
                    items,
                    empty: group.no_data.unwrap_or("").to_owned(),
                }),
            ));
        }
    }
    if can_use[1] {
        page = page.section(Section::new(
            "Active Repositories",
            Body::List(List {
                items: repositories(state, &viewer).await?,
                empty: "No repositories found.".to_owned(),
            }),
        ));
    }
    let listing = crate::feed::load(
        state,
        &viewer.user.viewer(),
        FeedQuery {
            cursor: Cursor::Newest,
            limit: 40,
            references: None,
        },
    )
    .await?;
    let stories: Vec<String> = crate::feed::items(&listing)
        .iter()
        .map(|story| {
            format!(
                "<li class=\"feed-story\"><span class=\"feed-title\">{}</span> <time \
                 class=\"muted\">{}</time></li>",
                story.title_html,
                escape(&story.date)
            )
        })
        .collect();
    let feed = if stories.is_empty() {
        "<p class=\"muted\">No stories.</p>".to_owned()
    } else {
        format!("<ul class=\"feed\">{}</ul>", stories.join(""))
    };
    page = page.section(Section::new("Recent Activity", Body::Html(feed)));
    for link in menu {
        page = page.side(link);
    }
    Ok(page.build(state, &viewer, &uri))
}
