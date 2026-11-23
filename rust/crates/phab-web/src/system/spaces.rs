//! `/spaces/` (`PhabricatorSpacesListController` with the builtin queries
//! of `PhabricatorSpacesNamespaceSearchEngine`) and `/S<id>`
//! (`PhabricatorSpacesViewController`). A space with a history (every
//! space created from the web has one) shows its timeline, which is PHP's.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::Uri;
use phab_app_harbormaster::web::has_named_queries;
use phab_core::Phid;
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use phab_policy::loader::{self, NoExtendedObjects};
use phab_policy::{Capability, ObjectKind, PolicySubject};

use super::{Body, Item, Link, List, PageBuilder, Property, Section, SystemPage, no_query};
use crate::error::PageError;
use crate::response::Page;
use crate::state::WebState;
use crate::viewer::WebUser;

/// `PhabricatorSpacesApplication`.
pub(crate) const APPLICATION: &str = "PhabricatorSpacesApplication";

/// `PhabricatorSpacesNamespaceSearchEngine`.
const ENGINE: &str = "PhabricatorSpacesNamespaceSearchEngine";

/// A `spaces_namespace` row.
struct Space {
    id: i64,
    phid: Phid,
    name: String,
    view_policy: String,
    edit_policy: String,
    is_default: bool,
    description: String,
    is_archived: bool,
}

impl Space {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            name: rows::text(row, "namespaceName")?,
            view_policy: rows::text(row, "viewPolicy")?,
            edit_policy: rows::text(row, "editPolicy")?,
            is_default: rows::opt_int(row, "isDefaultNamespace")?.unwrap_or(0) != 0,
            description: rows::text(row, "description")?,
            is_archived: rows::flag(row, "isArchived")?,
        })
    }

    fn subject(&self) -> PolicySubject {
        PolicySubject::new(Some(self.phid.clone()), ObjectKind::Other)
            .with_policy(Capability::VIEW, self.view_policy.clone())
            .with_policy(Capability::EDIT, self.edit_policy.clone())
    }
}

async fn load(database: &Database, condition: &str) -> Result<Vec<Space>, DbError> {
    let sql = format!(
        "SELECT id, phid, namespaceName, viewPolicy, editPolicy, isDefaultNamespace, \
         description, isArchived FROM {} {condition}",
        database.table("spaces", "spaces_namespace")
    );
    phab_db::sqlx::query(&sql)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(Space::from_row)
        .collect()
}

/// Which of `spaces` the viewer has `capability` on.
async fn allowed<S: WebState>(
    state: &S,
    viewer: &WebUser,
    spaces: &[Space],
    capability: Capability,
) -> Result<Vec<bool>, PageError> {
    let as_viewer = viewer.user.viewer();
    let subjects: Vec<PolicySubject> = spaces.iter().map(Space::subject).collect();
    let refs: Vec<&PolicySubject> = subjects.iter().collect();
    let facts = loader::load_facts(
        state.database(),
        state.config(),
        &as_viewer,
        &refs,
        &NoExtendedObjects,
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    phab_policy::filter(&as_viewer, &refs, &[capability], &facts)
        .map_err(|error| PageError::Internal(Box::new(error)))
}

/// `GET /spaces/`.
pub(crate) async fn list<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    if has_named_queries(state.database(), &viewer.user.phid, ENGINE).await? {
        return Err(PageError::Fallback("saved space queries"));
    }
    render_list(state.as_ref(), &viewer, "active", &uri).await
}

/// `GET /spaces/query/<key>/`.
pub(crate) async fn query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    let key = match key.as_str() {
        "active" => "active",
        "all" => "all",
        _ => return Err(PageError::Fallback("a saved space query")),
    };
    render_list(state.as_ref(), &viewer, key, &uri).await
}

async fn render_list<S: WebState>(
    state: &S,
    viewer: &WebUser,
    key: &'static str,
    uri: &Uri,
) -> Result<Page<SystemPage>, PageError> {
    no_query(uri)?;
    let condition = if key == "active" {
        "WHERE isArchived = 0 ORDER BY id DESC"
    } else {
        "ORDER BY id DESC"
    };
    let spaces = load(state.database(), condition).await?;
    let visible = allowed(state, viewer, &spaces, Capability::VIEW).await?;
    let spaces: Vec<Space> = spaces
        .into_iter()
        .zip(visible)
        .filter_map(|(space, visible)| visible.then_some(space))
        .collect();
    if spaces.len() > 100 {
        return Err(PageError::Fallback("more than a page of spaces"));
    }
    let items = spaces
        .iter()
        .map(|space| Item {
            name: format!("S{} {}", space.id, space.name),
            uri: Some(format!("/S{}", space.id)),
            icon: space.is_default.then(|| "fa-certificate".to_owned()),
            tags: if space.is_default {
                vec!["Default Space".to_owned()]
            } else {
                Vec::new()
            },
            attributes_html: Vec::new(),
            disabled: space.is_archived,
        })
        .collect();
    Ok(PageBuilder::new("Spaces")
        .title(if key == "active" {
            "Active Spaces"
        } else {
            "All Spaces"
        })
        .icon("fa-th-large")
        .nav(vec![
            Link::new("Active Spaces", "/spaces/query/active/").inactive(key == "active"),
            Link::new("All Spaces", "/spaces/query/all/").inactive(key == "all"),
            Link::new("Create Space", "/spaces/create/").icon("fa-plus"),
        ])
        .section(Section::untitled(Body::List(List {
            items,
            empty: "No spaces found.".to_owned(),
        })))
        .build(state, viewer, uri))
}

/// `GET /S<id>`, reached through the monogram route.
pub(crate) async fn view<S: WebState>(
    state: &Arc<S>,
    viewer: WebUser,
    id: i64,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    viewer.require_application(&**state, APPLICATION).await?;
    no_query(&uri)?;
    let state = state.as_ref();
    let database = state.database();
    let space = load(database, &format!("WHERE id = {id}"))
        .await?
        .pop()
        .ok_or(PageError::NotFound)?;
    let spaces = [space];
    if !allowed(state, &viewer, &spaces, Capability::VIEW).await?[0] {
        return Err(PageError::NotFound);
    }
    let [space] = spaces;
    let sql = format!(
        "SELECT 1 FROM {} WHERE objectPHID = ? LIMIT 1",
        database.table("spaces", "spaces_namespacetransaction")
    );
    if phab_db::sqlx::query(&sql)
        .bind(space.phid.as_str())
        .fetch_optional(database.pool())
        .await
        .map_err(DbError::from)?
        .is_some()
    {
        return Err(PageError::Fallback("the history of a space"));
    }
    let spaces = [space];
    let can_edit = allowed(state, &viewer, &spaces, Capability::EDIT).await?[0];
    let [space] = spaces;
    let mut page = PageBuilder::new(&space.name)
        .title(format!("S{} {}", space.id, space.name))
        .icon("fa-th-large")
        .tag(if space.is_archived {
            "Archived"
        } else {
            "Active"
        })
        .section(Section::new(
            "Details",
            Body::Properties(vec![Property::text(
                "Default Space",
                if space.is_default { "Yes" } else { "No" },
            )]),
        ));
    if !space.description.is_empty() {
        page = page.section(Section::new(
            "Description",
            Body::Html(super::remarkup_html(state, &viewer, &space.description).await?),
        ));
    }
    page = page.side(
        Link::new("Edit Space", format!("/spaces/edit/{}/", space.id))
            .icon("fa-pencil")
            .inactive(!can_edit),
    );
    page = if space.is_archived {
        page.side(
            Link::new("Activate Space", format!("/spaces/activate/{}/", space.id))
                .icon("fa-check")
                .inactive(!can_edit),
        )
    } else {
        page.side(
            Link::new("Archive Space", format!("/spaces/archive/{}/", space.id))
                .icon("fa-ban")
                .inactive(!can_edit),
        )
    };
    Ok(page.build(state, &viewer, &uri))
}
