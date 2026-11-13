//! Remarkup for the pages: the engine set up from the Phabricator
//! configuration, with object references (`D123`, `#project`, `@user`)
//! resolved as the viewer may see them (`PhabricatorMarkupEngine` with a
//! viewer).

use std::collections::HashMap;

use phab_app_differential::storage;
use phab_app_project as project;
use phab_core::Config;
use phab_policy::Viewer;
use phab_remarkup::{Engine, Markup, MentionedUser, Mode, ObjectHandle, Prepared, Resolved};

use crate::error::PageError;
use crate::state::WebState;

/// The engine options from the configuration
/// (`getMarkupEngineDefaultConfiguration()`).
pub fn engine_config(config: &Config) -> phab_remarkup::Config {
    let allowed_protocols = config
        .object("uri.allowed-protocols")
        .map(|protocols| {
            protocols
                .iter()
                .filter(|(_, enabled)| phab_core::php::is_truthy(enabled))
                .map(|(protocol, _)| protocol.clone())
                .collect()
        })
        .unwrap_or_default();
    let string = |key: &str| config.nonempty_string(key).map(str::to_owned);
    phab_remarkup::Config {
        allowed_protocols,
        base_uri: string("phabricator.base-uri"),
        production_uri: string("phabricator.production-uri"),
        ignored_object_names: string("remarkup.ignored-object-names"),
        bugzilla_url: string("bugzilla.url"),
        ..phab_remarkup::phabricator::default_config()
    }
}

/// The engine of the pages.
pub fn new_engine(config: &Config) -> Engine {
    phab_remarkup::phabricator::engine(engine_config(config))
}

/// Renders `text` for `viewer` (for a logged-in user, `user.viewer()`).
pub async fn render<S: WebState>(
    state: &S,
    viewer: &Viewer,
    text: &str,
    mode: Mode,
) -> Result<Markup, PageError> {
    let engine = state.remarkup();
    let prepared = engine.prepare(text.as_bytes(), mode);
    let resolved = resolve(state, viewer, &prepared).await?;
    Ok(engine.finish(prepared, resolved).output)
}

/// Resolves `#hashtag` references: hashtags match without case, like
/// `PhabricatorProjectProjectPHIDType::loadNamedObjects()`.
async fn resolve_projects<S: WebState>(
    state: &S,
    viewer: &Viewer,
    prepared: &Prepared<'_>,
    resolved: &mut Resolved,
) -> Result<(), PageError> {
    let written: Vec<(Vec<u8>, String)> = prepared
        .object_references()
        .into_iter()
        .filter(|(prefix, _)| *prefix == "#")
        .filter_map(|(_, id)| {
            let slug = String::from_utf8(id.clone()).ok()?.to_lowercase();
            Some((id, slug))
        })
        .collect();
    if written.is_empty() {
        return Ok(());
    }
    let mut slugs: Vec<String> = written.iter().map(|(_, slug)| slug.clone()).collect();
    slugs.sort();
    slugs.dedup();
    let found = project::storage::load_by_slug(state.database(), &slugs).await?;
    let (visible, graph) = project::policy::visible_projects(
        state.database(),
        state.config(),
        viewer,
        found.iter().map(|(_, project)| project.clone()).collect(),
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    let visible: HashMap<&phab_core::Phid, &project::Project> = visible
        .iter()
        .map(|project| (&project.phid, project))
        .collect();
    for (id, slug) in written {
        let Some(project) = found
            .iter()
            .find(|(found_slug, _)| *found_slug == slug)
            .and_then(|(_, project)| visible.get(&project.phid))
        else {
            continue;
        };
        let name = project.display_name(graph.parent(project));
        resolved.objects.insert(
            ("#", id),
            ObjectHandle {
                phid: project.phid.as_str().to_owned(),
                name: name.clone().into_bytes(),
                full_name: name.into_bytes(),
                uri: project.uri().into_bytes(),
                closed: project.is_archived(),
            },
        );
    }
    Ok(())
}

/// Loads what the references of `prepared` point to, keeping what
/// `viewer` may see.
async fn resolve<S: WebState>(
    state: &S,
    viewer: &Viewer,
    prepared: &Prepared<'_>,
) -> Result<Resolved, PageError> {
    let mut resolved = Resolved::default();

    let ids: Vec<(Vec<u8>, i64)> = prepared
        .object_references()
        .into_iter()
        .filter(|(prefix, _)| *prefix == "D")
        .filter_map(|(_, id)| {
            let number = std::str::from_utf8(&id).ok()?.parse().ok()?;
            Some((id, number))
        })
        .collect();
    if !ids.is_empty() {
        let numbers: Vec<i64> = ids.iter().map(|(_, number)| *number).collect();
        let revisions = storage::load_revisions_by_id(state.database(), &numbers).await?;
        let revisions = storage::visible_revisions(
            state.database(),
            state.config(),
            viewer,
            revisions,
            state.now(),
        )
        .await
        .map_err(|error| PageError::Internal(Box::new(error)))?;
        let by_id: HashMap<i64, storage::Revision> = revisions
            .into_iter()
            .map(|revision| (revision.id, revision))
            .collect();
        for (written, number) in ids {
            let Some(revision) = by_id.get(&number) else {
                continue;
            };
            let monogram = format!("D{}", revision.id);
            resolved.objects.insert(
                ("D", written),
                ObjectHandle {
                    phid: revision.phid.as_str().to_owned(),
                    name: monogram.clone().into_bytes(),
                    full_name: format!("{monogram}: {}", revision.title).into_bytes(),
                    uri: format!("/{monogram}").into_bytes(),
                    closed: revision.is_closed(),
                },
            );
        }
    }

    resolve_projects(state, viewer, prepared, &mut resolved).await?;

    // Users are visible to everyone who can log in.
    let usernames: Vec<String> = prepared
        .mentioned_usernames()
        .into_iter()
        .filter_map(|name| String::from_utf8(name).ok())
        .collect();
    let names: Vec<&str> = usernames.iter().map(String::as_str).collect();
    for user in phab_auth::user::load_users_by_username(state.database(), &names).await? {
        resolved.users.insert(
            user.user_name.to_lowercase().into_bytes(),
            MentionedUser {
                phid: user.phid.as_str().to_owned(),
                user_name: user.user_name.clone().into_bytes(),
                is_disabled: user.is_disabled,
                can_view_context: true,
            },
        );
    }
    Ok(resolved)
}
