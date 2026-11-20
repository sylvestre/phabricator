//! Remarkup for the pages: the engine set up from the Phabricator
//! configuration, with object references (`D123`, `#project`, `@user`,
//! `F1`, `H1`, `rMOZ`, commits, `{PHID-...}`, `{src}`, links to the
//! install) resolved as the viewer may see them (`PhabricatorMarkupEngine`
//! with a viewer).
//!
//! `{PHID-...}` handles show revisions, projects, users, repositories and
//! files; other objects stay as written.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use phab_app_differential::repository::{self, Repository};
use phab_app_differential::{commit, storage};
use phab_app_files as files;
use phab_app_herald as herald;
use phab_app_project as project;
use phab_core::{Config, Phid, PhidType};
use phab_policy::Viewer;
use phab_remarkup::objects::Preview;
use phab_remarkup::{
    Engine, FileObject, Highlighter, Markup, MentionedUser, Mode, ObjectHandle, Prepared,
    RepositoryRef, Resolved,
};

use crate::error::PageError;
use crate::searchfox::Trees;
use crate::state::WebState;

/// The engine options from the configuration
/// (`getMarkupEngineDefaultConfiguration()`), with syntax highlighting.
pub fn engine_config(config: &Config) -> phab_remarkup::Config {
    phab_remarkup::Config {
        highlighter: Some(Highlighter(Arc::new(|language: &[u8], code: &[u8]| {
            crate::highlight::highlight_code(language, code).map(Markup::html)
        }))),
        ..phab_app_differential::markup::engine_config(config)
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
    let mut rendered = render_all(state, viewer, &[text], mode).await?;
    Ok(rendered.pop().expect("one text gives one rendering"))
}

/// Renders each of `texts` for `viewer`, resolving the references of all
/// of them together: a page of comments costs the same few queries as one
/// comment (`PhabricatorMarkupEngine::process()` with several objects).
pub async fn render_all<S: WebState>(
    state: &S,
    viewer: &Viewer,
    texts: &[&str],
    mode: Mode,
) -> Result<Vec<Markup>, PageError> {
    let engine = state.remarkup();
    let prepared: Vec<Prepared<'_>> = texts
        .iter()
        .map(|text| engine.prepare(text.as_bytes(), mode))
        .collect();
    let resolved = resolve(state, viewer, &prepared).await?;
    Ok(prepared
        .into_iter()
        .map(|prepared| engine.finish(prepared, resolved.clone()))
        .map(|rendered| rendered.output)
        .collect())
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> PageError {
    PageError::Internal(Box::new(error))
}

/// What the texts refer to, gathered from all of them.
#[derive(Default)]
struct Wanted {
    /// Object references by rule prefix: the IDs as written.
    references: HashMap<&'static str, Vec<Vec<u8>>>,
    /// `{PHID-...}` handles by PHID type.
    handles: HashMap<PhidType, Vec<Phid>>,
    /// Lowercased usernames.
    usernames: Vec<String>,
    /// Repositories of source links, by identifier as written.
    repositories: Vec<Vec<u8>>,
}

impl Wanted {
    fn gather(prepared: &[Prepared<'_>]) -> Self {
        let mut wanted = Self::default();
        for text in prepared {
            for (prefix, id) in text.object_references() {
                let ids = wanted.references.entry(prefix).or_default();
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            for phid in text.handle_phids() {
                let phid = Phid::new(phid);
                let phids = wanted.handles.entry(phid.phid_type()).or_default();
                if !phids.contains(&phid) {
                    phids.push(phid);
                }
            }
            for name in text.mentioned_usernames() {
                if let Ok(name) = String::from_utf8(name)
                    && !wanted.usernames.contains(&name)
                {
                    wanted.usernames.push(name);
                }
            }
            for identifier in text.repository_identifiers() {
                if !wanted.repositories.contains(&identifier) {
                    wanted.repositories.push(identifier);
                }
            }
        }
        wanted
    }

    fn ids(&self, prefix: &str) -> &[Vec<u8>] {
        self.references.get(prefix).map_or(&[], Vec::as_slice)
    }

    /// The IDs of numeric references, with the ID as written.
    fn numeric_ids(&self, prefix: &str) -> Vec<(Vec<u8>, i64)> {
        self.ids(prefix)
            .iter()
            .filter_map(|id| Some((id.clone(), std::str::from_utf8(id).ok()?.parse().ok()?)))
            .collect()
    }

    fn handles(&self, phid_type: PhidType) -> &[Phid] {
        self.handles.get(&phid_type).map_or(&[], Vec::as_slice)
    }
}

/// Loads what the references of `prepared` point to, keeping what
/// `viewer` may see.
async fn resolve<S: WebState>(
    state: &S,
    viewer: &Viewer,
    prepared: &[Prepared<'_>],
) -> Result<Resolved, PageError> {
    let wanted = Wanted::gather(prepared);
    let mut resolved = Resolved::default();
    resolve_revisions(state, viewer, &wanted, &mut resolved).await?;
    resolve_projects(state, viewer, &wanted, &mut resolved).await?;
    resolve_users(state, &wanted, &mut resolved).await?;
    resolve_repositories(state, viewer, &wanted, &mut resolved).await?;
    resolve_commits(state, viewer, &wanted, &mut resolved).await?;
    resolve_files(state, viewer, &wanted, &mut resolved).await?;
    resolve_herald_rules(state, viewer, &wanted, &mut resolved).await?;
    resolve_spaces(state, viewer, &wanted, &mut resolved).await?;
    Ok(resolved)
}

fn revision_handle(revision: &storage::Revision) -> ObjectHandle {
    let monogram = format!("D{}", revision.id);
    ObjectHandle {
        phid: revision.phid.as_str().to_owned(),
        name: monogram.clone().into_bytes(),
        full_name: format!("{monogram}: {}", revision.title).into_bytes(),
        uri: format!("/{monogram}").into_bytes(),
        closed: revision.is_closed(),
        reference_text: None,
    }
}

/// `D123` and `{PHID-DREV-...}`.
async fn resolve_revisions<S: WebState>(
    state: &S,
    viewer: &Viewer,
    wanted: &Wanted,
    resolved: &mut Resolved,
) -> Result<(), PageError> {
    let ids = wanted.numeric_ids("D");
    let phids = wanted.handles(PhidType::REVISION);
    if ids.is_empty() && phids.is_empty() {
        return Ok(());
    }
    let numbers: Vec<i64> = ids.iter().map(|(_, number)| *number).collect();
    let mut revisions = storage::load_revisions_by_id(state.database(), &numbers).await?;
    revisions.extend(storage::load_revisions(state.database(), phids).await?);
    let revisions = storage::visible_revisions(
        state.database(),
        state.config(),
        viewer,
        revisions,
        state.now(),
    )
    .await
    .map_err(internal)?;
    for revision in &revisions {
        let handle = revision_handle(revision);
        if phids.contains(&revision.phid) {
            resolved
                .handles
                .insert(revision.phid.as_str().to_owned(), handle.clone());
        }
        for (written, _) in ids.iter().filter(|(_, number)| *number == revision.id) {
            resolved
                .objects
                .insert(("D", written.clone()), handle.clone());
        }
    }
    Ok(())
}

/// `#hashtag` and `{PHID-PROJ-...}`: hashtags match without case, like
/// `PhabricatorProjectProjectPHIDType::loadNamedObjects()`.
async fn resolve_projects<S: WebState>(
    state: &S,
    viewer: &Viewer,
    wanted: &Wanted,
    resolved: &mut Resolved,
) -> Result<(), PageError> {
    let written: Vec<(Vec<u8>, String)> = wanted
        .ids("#")
        .iter()
        .filter_map(|id| {
            let slug = String::from_utf8(id.clone()).ok()?.to_lowercase();
            Some((id.clone(), slug))
        })
        .collect();
    let phids = wanted.handles(PhidType::PROJECT);
    if written.is_empty() && phids.is_empty() {
        return Ok(());
    }
    let mut slugs: Vec<String> = written.iter().map(|(_, slug)| slug.clone()).collect();
    slugs.sort();
    slugs.dedup();
    let found = project::storage::load_by_slug(state.database(), &slugs).await?;
    let mut candidates: Vec<project::Project> =
        found.iter().map(|(_, project)| project.clone()).collect();
    candidates.extend(project::storage::load_by_phid(state.database(), phids).await?);
    let (visible, graph) = project::policy::visible_projects(
        state.database(),
        state.config(),
        viewer,
        candidates,
        state.now(),
    )
    .await
    .map_err(internal)?;
    let visible: HashMap<&Phid, &project::Project> = visible
        .iter()
        .map(|project| (&project.phid, project))
        .collect();
    let handle = |project: &project::Project| {
        let name = project.display_name(graph.parent(project));
        ObjectHandle {
            phid: project.phid.as_str().to_owned(),
            name: name.clone().into_bytes(),
            full_name: name.into_bytes(),
            uri: project.uri().into_bytes(),
            closed: project.is_archived(),
            reference_text: None,
        }
    };
    for (id, slug) in written {
        let Some(project) = found
            .iter()
            .find(|(found_slug, _)| *found_slug == slug)
            .and_then(|(_, project)| visible.get(&project.phid))
        else {
            continue;
        };
        resolved.objects.insert(("#", id), handle(project));
    }
    for phid in phids {
        if let Some(project) = visible.get(phid) {
            resolved
                .handles
                .insert(phid.as_str().to_owned(), handle(project));
        }
    }
    Ok(())
}

/// `@username`, links to profiles and `{PHID-USER-...}`. Users are visible
/// to everyone who can log in.
async fn resolve_users<S: WebState>(
    state: &S,
    wanted: &Wanted,
    resolved: &mut Resolved,
) -> Result<(), PageError> {
    let names: Vec<&str> = wanted.usernames.iter().map(String::as_str).collect();
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
    let phids = wanted.handles(PhidType::USER);
    for user in phab_auth::user::load_users_by_phid(state.database(), phids).await? {
        resolved.handles.insert(
            user.phid.as_str().to_owned(),
            ObjectHandle {
                phid: user.phid.as_str().to_owned(),
                name: user.user_name.clone().into_bytes(),
                full_name: format!("{} ({})", user.user_name, user.real_name).into_bytes(),
                uri: format!("/p/{}/", user.user_name).into_bytes(),
                closed: user.is_disabled,
                reference_text: None,
            },
        );
    }
    Ok(())
}

fn repository_handle(repository: &Repository) -> ObjectHandle {
    let monogram = repository.monogram();
    ObjectHandle {
        phid: repository.phid.as_str().to_owned(),
        name: monogram.clone().into_bytes(),
        full_name: format!("{monogram} {}", repository.name).into_bytes(),
        uri: repository.uri().into_bytes(),
        closed: !repository.is_tracked(),
        reference_text: None,
    }
}

fn repository_ref(trees: &Trees, repository: &Repository) -> RepositoryRef {
    RepositoryRef {
        phid: repository.phid.as_str().to_owned(),
        monogram: repository.monogram().into_bytes(),
        uri: repository.uri().into_bytes(),
        default_branch: repository.default_branch().map(String::into_bytes),
        searchfox: trees
            .tree_for(repository)
            .map(|tree| tree.root().into_bytes()),
    }
}

/// `rCALLSIGN`, `R12`, `{PHID-REPO-...}` and the repositories of source
/// links.
async fn resolve_repositories<S: WebState>(
    state: &S,
    viewer: &Viewer,
    wanted: &Wanted,
    resolved: &mut Resolved,
) -> Result<(), PageError> {
    // The rules load their IDs as identifiers: "rMOZ" is the callsign
    // "MOZ", "R12" the ID "12".
    let mut identifiers: Vec<(Option<&'static str>, String)> = Vec::new();
    for prefix in ["r", "R"] {
        for id in wanted.ids(prefix) {
            if let Ok(id) = String::from_utf8(id.clone()) {
                identifiers.push((Some(prefix), id));
            }
        }
    }
    for identifier in &wanted.repositories {
        if let Ok(identifier) = String::from_utf8(identifier.clone()) {
            identifiers.push((None, identifier));
        }
    }
    let phids = wanted.handles(PhidType::REPOSITORY);
    if identifiers.is_empty() && phids.is_empty() {
        return Ok(());
    }
    let mut names: Vec<String> = identifiers.iter().map(|(_, name)| name.clone()).collect();
    names.sort();
    names.dedup();
    let by_identifier = repository::load_by_identifiers(state.database(), &names).await?;
    let mut candidates: Vec<Repository> = by_identifier.values().cloned().collect();
    candidates.extend(repository::load(state.database(), phids).await?);
    let visible: HashSet<Phid> = repository::visible(
        state.database(),
        state.config(),
        viewer,
        candidates.clone(),
        state.now(),
    )
    .await
    .map_err(internal)?
    .into_iter()
    .map(|repository| repository.phid)
    .collect();
    let trees = state.searchfox_trees();
    for (prefix, identifier) in identifiers {
        let Some(repository) = by_identifier
            .get(&identifier)
            .filter(|repository| visible.contains(&repository.phid))
        else {
            continue;
        };
        match prefix {
            Some(prefix) => {
                resolved.objects.insert(
                    (prefix, identifier.into_bytes()),
                    repository_handle(repository),
                );
            }
            None => {
                resolved
                    .repositories
                    .insert(identifier.into_bytes(), repository_ref(trees, repository));
            }
        }
    }
    for repository in candidates {
        if phids.contains(&repository.phid) && visible.contains(&repository.phid) {
            resolved.handles.insert(
                repository.phid.as_str().to_owned(),
                repository_handle(&repository),
            );
        }
    }
    Ok(())
}

/// Commits: `rMOZabcdef`, `R12:abcdef` and bare hashes.
async fn resolve_commits<S: WebState>(
    state: &S,
    viewer: &Viewer,
    wanted: &Wanted,
    resolved: &mut Resolved,
) -> Result<(), PageError> {
    let identifiers: Vec<String> = wanted
        .ids("")
        .iter()
        .filter_map(|id| String::from_utf8(id.clone()).ok())
        .collect();
    if identifiers.is_empty() {
        return Ok(());
    }
    let commits = commit::load_visible_by_identifiers(
        state.database(),
        state.config(),
        viewer,
        &identifiers,
        state.now(),
    )
    .await
    .map_err(internal)?;
    for (identifier, commit) in commits {
        let name = commit.name();
        resolved.objects.insert(
            ("", identifier.into_bytes()),
            ObjectHandle {
                phid: commit.phid.as_str().to_owned(),
                name: name.clone().into_bytes(),
                full_name: commit.full_name().into_bytes(),
                uri: commit.uri().into_bytes(),
                closed: commit.is_unreachable,
                // A rewritten commit shows its name rather than what was
                // written (T11522).
                reference_text: commit.is_unreachable.then(|| name.into_bytes()),
            },
        );
    }
    Ok(())
}

/// The files of `candidates` the viewer may see: those whose policy lets
/// them, those they uploaded, those attached to a revision they can see,
/// and thumbnails of files they can see (`PhabricatorFileQuery`).
async fn visible_files<S: WebState>(
    state: &S,
    viewer: &Viewer,
    candidates: Vec<files::File>,
) -> Result<Vec<files::File>, PageError> {
    let database = state.database();
    let attachments = files::attached_objects(database, &candidates).await?;
    let attached: Vec<Phid> = attachments
        .values()
        .flatten()
        .filter(|phid| phid.phid_type() == PhidType::REVISION)
        .cloned()
        .collect();
    let revisions = storage::load_revisions(database, &attached).await?;
    let visible_objects: HashSet<Phid> =
        storage::visible_revisions(database, state.config(), viewer, revisions, state.now())
            .await
            .map_err(internal)?
            .into_iter()
            .map(|revision| revision.phid)
            .collect();
    let originals = files::originals(database, &candidates).await?;
    // A thumbnail goes with its original; an original's own originals
    // are not followed.
    let original_files =
        files::load_by_phids(database, &originals.values().cloned().collect::<Vec<_>>()).await?;
    let original_attachments = files::attached_objects(database, &original_files).await?;
    let original_revisions: Vec<Phid> = original_attachments
        .values()
        .flatten()
        .filter(|phid| phid.phid_type() == PhidType::REVISION)
        .cloned()
        .collect();
    let original_revisions = storage::load_revisions(database, &original_revisions).await?;
    let original_objects: HashSet<Phid> = storage::visible_revisions(
        database,
        state.config(),
        viewer,
        original_revisions,
        state.now(),
    )
    .await
    .map_err(internal)?
    .into_iter()
    .map(|revision| revision.phid)
    .collect();
    let attached_to_visible =
        |attachments: &HashMap<Phid, Vec<Phid>>, objects: &HashSet<Phid>, file: &files::File| {
            attachments
                .get(&file.phid)
                .is_some_and(|phids| phids.iter().any(|phid| objects.contains(phid)))
        };
    let original_set: HashSet<Phid> = original_files
        .iter()
        .filter(|file| attached_to_visible(&original_attachments, &original_objects, file))
        .map(|file| file.phid.clone())
        .collect();
    let visible_originals: HashSet<Phid> = files::visible(
        database,
        state.config(),
        viewer,
        original_files,
        &original_set,
        state.now(),
    )
    .await
    .map_err(internal)?
    .into_iter()
    .map(|file| file.phid)
    .collect();
    let mut granted = HashSet::new();
    let mut kept = Vec::new();
    for file in candidates {
        match originals.get(&file.phid) {
            // PHID-VOID marks builtin thumbnails.
            Some(original) if original.phid_type() != PhidType::VOID => {
                if !visible_originals.contains(original) {
                    continue;
                }
                granted.insert(file.phid.clone());
            }
            _ => {}
        }
        if attached_to_visible(&attachments, &visible_objects, &file) {
            granted.insert(file.phid.clone());
        }
        kept.push(file);
    }
    files::visible(
        database,
        state.config(),
        viewer,
        kept,
        &granted,
        state.now(),
    )
    .await
    .map_err(internal)
}

fn file_handle(file: &files::File) -> ObjectHandle {
    let monogram = file.monogram();
    ObjectHandle {
        phid: file.phid.as_str().to_owned(),
        name: monogram.clone().into_bytes(),
        full_name: format!("{monogram}: {}", file.name).into_bytes(),
        uri: format!("/{monogram}").into_bytes(),
        closed: false,
        reference_text: None,
    }
}

/// `F123`, `{F123}` and `{PHID-FILE-...}`.
async fn resolve_files<S: WebState>(
    state: &S,
    viewer: &Viewer,
    wanted: &Wanted,
    resolved: &mut Resolved,
) -> Result<(), PageError> {
    let ids = wanted.numeric_ids("F");
    let phids = wanted.handles(PhidType::FILE);
    if ids.is_empty() && phids.is_empty() {
        return Ok(());
    }
    let numbers: Vec<i64> = ids.iter().map(|(_, number)| *number).collect();
    let mut candidates = files::load_by_ids(state.database(), &numbers).await?;
    candidates.extend(files::load_by_phids(state.database(), phids).await?);
    let visible = visible_files(state, viewer, candidates).await?;
    let previews = files::transforms(state.database(), &visible, files::TRANSFORM_PREVIEW).await?;
    let config = state.config();
    for file in &visible {
        let handle = file_handle(file);
        if phids.contains(&file.phid) {
            resolved
                .handles
                .insert(file.phid.as_str().to_owned(), handle.clone());
        }
        let is_image = file.is_viewable_image(config);
        let preview = is_image.then(|| Preview {
            uri: previews
                .get(&file.phid)
                .map_or_else(
                    || file.transform_uri(config, files::TRANSFORM_PREVIEW),
                    |preview| preview.view_uri(config),
                )
                .into_bytes(),
            size: file
                .preview_size(config)
                .map(|(width, height)| (width.into_bytes(), height.into_bytes())),
        });
        let object = FileObject {
            name: file.name.clone().into_bytes(),
            mime_type: file.mime_type.clone().into_bytes(),
            best_uri: file.best_uri(config).into_bytes(),
            download_uri: file.download_uri(config).into_bytes(),
            preview,
            image_size: file.image_size(config),
            is_viewable_image: is_image,
            is_audio: file.is_audio(config),
            is_video: file.is_video(config),
            byte_size: file.byte_size,
            alt_text: file.alt_text(config).into_bytes(),
        };
        for (written, _) in ids.iter().filter(|(_, number)| *number == file.id) {
            resolved
                .objects
                .insert(("F", written.clone()), handle.clone());
            resolved.files.insert(written.clone(), object.clone());
        }
    }
    Ok(())
}

/// `H123`.
async fn resolve_herald_rules<S: WebState>(
    state: &S,
    viewer: &Viewer,
    wanted: &Wanted,
    resolved: &mut Resolved,
) -> Result<(), PageError> {
    let ids = wanted.numeric_ids("H");
    if ids.is_empty() {
        return Ok(());
    }
    let numbers: Vec<i64> = ids.iter().map(|(_, number)| *number).collect();
    let rules = herald::load_by_ids(state.database(), &numbers).await?;
    // Object rules are visible with their project or repository.
    let triggers: Vec<Phid> = rules
        .iter()
        .filter(|rule| rule.is_object_rule())
        .filter_map(|rule| rule.trigger_object_phid.clone())
        .collect();
    let mut visible_triggers = HashSet::new();
    let projects: Vec<Phid> = triggers
        .iter()
        .filter(|phid| phid.phid_type() == PhidType::PROJECT)
        .cloned()
        .collect();
    if !projects.is_empty() {
        let projects = project::storage::load_by_phid(state.database(), &projects).await?;
        let (projects, _) = project::policy::visible_projects(
            state.database(),
            state.config(),
            viewer,
            projects,
            state.now(),
        )
        .await
        .map_err(internal)?;
        visible_triggers.extend(projects.into_iter().map(|project| project.phid));
    }
    let repositories: Vec<Phid> = triggers
        .iter()
        .filter(|phid| phid.phid_type() == PhidType::REPOSITORY)
        .cloned()
        .collect();
    if !repositories.is_empty() {
        let repositories = repository::load(state.database(), &repositories).await?;
        let repositories = repository::visible(
            state.database(),
            state.config(),
            viewer,
            repositories,
            state.now(),
        )
        .await
        .map_err(internal)?;
        visible_triggers.extend(repositories.into_iter().map(|repository| repository.phid));
    }
    let rules = herald::visible(
        state.database(),
        state.config(),
        viewer,
        rules,
        &visible_triggers,
        state.now(),
    )
    .await
    .map_err(internal)?;
    for rule in rules {
        let monogram = rule.monogram();
        let handle = ObjectHandle {
            phid: rule.phid.as_str().to_owned(),
            name: monogram.clone().into_bytes(),
            full_name: format!("{monogram} {}", rule.name).into_bytes(),
            uri: format!("/{monogram}").into_bytes(),
            closed: rule.is_disabled,
            reference_text: None,
        };
        for (written, _) in ids.iter().filter(|(_, number)| *number == rule.id) {
            resolved
                .objects
                .insert(("H", written.clone()), handle.clone());
        }
    }
    Ok(())
}

/// `S1`.
async fn resolve_spaces<S: WebState>(
    state: &S,
    viewer: &Viewer,
    wanted: &Wanted,
    resolved: &mut Resolved,
) -> Result<(), PageError> {
    let ids = wanted.numeric_ids("S");
    if ids.is_empty() {
        return Ok(());
    }
    let numbers: Vec<i64> = ids.iter().map(|(_, number)| *number).collect();
    let spaces = herald::spaces::load_by_ids(state.database(), &numbers).await?;
    let spaces = herald::spaces::visible(
        state.database(),
        state.config(),
        viewer,
        spaces,
        state.now(),
    )
    .await
    .map_err(internal)?;
    for space in spaces {
        let monogram = space.monogram();
        let handle = ObjectHandle {
            phid: space.phid.as_str().to_owned(),
            name: space.name.clone().into_bytes(),
            full_name: format!("{monogram} {}", space.name).into_bytes(),
            uri: format!("/{monogram}").into_bytes(),
            closed: space.is_archived,
            reference_text: None,
        };
        for (written, _) in ids.iter().filter(|(_, number)| *number == space.id) {
            resolved
                .objects
                .insert(("S", written.clone()), handle.clone());
        }
    }
    Ok(())
}
