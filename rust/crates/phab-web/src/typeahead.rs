//! Suggestions for the fields naming users and projects
//! (`PhabricatorTypeaheadModularDatasourceController` with the reviewer
//! datasource): usernames and hashtags starting with what was typed.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use phab_app_project as project;
use serde::Serialize;

use crate::error::PageError;
use crate::state::WebState;
use crate::viewer::WebUser;

/// Suggestions of each kind.
const LIMIT: u32 = 8;

/// `?q=`.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct Params {
    #[serde(default)]
    q: String,
}

/// One suggestion.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct Suggestion {
    /// What the field gets: `alice` or `#team`.
    value: String,
    /// What the list shows.
    label: String,
}

/// `GET /rust-ui/typeahead?q=al`: users, then projects the viewer may see.
pub(crate) async fn suggest<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Query(params): Query<Params>,
) -> Result<Json<Vec<Suggestion>>, PageError> {
    let query = params.q.trim().trim_start_matches('@');
    let mut suggestions = Vec::new();
    if query.is_empty() {
        return Ok(Json(suggestions));
    }
    let database = state.database();
    if let Some(slug) = query.strip_prefix('#') {
        if !slug.is_empty() {
            let found = project::storage::search_by_slug_prefix(database, slug, LIMIT).await?;
            let (visible, graph) = project::policy::visible_projects(
                database,
                state.config(),
                &viewer.user.viewer(),
                found.iter().map(|(_, project)| project.clone()).collect(),
                state.now(),
            )
            .await
            .map_err(|error| PageError::Internal(Box::new(error)))?;
            for (slug, project) in &found {
                if let Some(project) = visible.iter().find(|shown| shown.phid == project.phid) {
                    suggestions.push(Suggestion {
                        value: format!("#{slug}"),
                        label: project.display_name(graph.parent(project)),
                    });
                }
            }
        }
        return Ok(Json(suggestions));
    }
    for user in phab_auth::user::search_by_username_prefix(database, query, LIMIT).await? {
        let label = if user.real_name.is_empty() {
            user.user_name.clone()
        } else {
            format!("{} ({})", user.user_name, user.real_name)
        };
        suggestions.push(Suggestion {
            value: user.user_name,
            label,
        });
    }
    Ok(Json(suggestions))
}
