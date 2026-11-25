//! The forms which create a diff from the text of a raw diff
//! (`DifferentialDiffCreateController`): `/differential/diff/create/`, and
//! `/differential/revision/update/<id>/`, the same form for a revision the
//! viewer can edit, whose repository it starts with. A post creates the
//! diff as PHP's form does, through `differential.createrawdiff` (the
//! Rust method, called as the viewer with its Conduit content source, as
//! PHP's `ConduitCall` does), and redirects to the diff's page
//! (`?revisionID=` for an update, which its "Attach To" form selects).
//!
//! The form posts PHP's fields: `diff`, the repository tokenizer
//! (`differential:repository[]`, whose first value replaces the default)
//! and `viewPolicy`. Without JavaScript tokenizers, the Rust form lists the
//! repository as a checkbox, and a name typed in `rust.add.repository`
//! shows the form again with it, without writing. An empty diff shows the
//! form again with PHP's error.
//!
//! Left to PHP (`421`, before writing): uploaded files (`diff-file`, a
//! `multipart/form-data` post, which Rust doesn't read), posts which aren't
//! form posts, workflow requests, invalid CSRF tokens, read-only installs,
//! revisions the viewer can't see or edit, and what
//! `differential.createrawdiff` leaves to PHP or refuses (PHP shows its
//! exception, or the form with the validation errors).

use std::sync::Arc;

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Router, routing};
use phab_app_differential::create_raw_diff::CreateRawDiff;
use phab_app_differential::repository;
use phab_app_differential::storage::{self, Revision};
use phab_conduit::{Method as _, MethodError};
use phab_core::{Config, Phid};
use phab_policy::application;
use serde_json::{Map, json};

use crate::error::PageError;
use crate::login::request::LoginRequest;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::web_edit;

/// `DifferentialRepositoryField::getFieldKey()`, the tokenizer's name.
const REPOSITORY_KEY: &str = "differential:repository";

/// PHP's error for a post without a diff.
const EMPTY_DIFF: &str =
    "You can not create an empty diff. Paste a diff or upload a file containing a diff.";

/// The repository of the form.
pub(crate) struct RepositoryToken {
    phid: String,
    label: String,
}

/// An option of the view policy control.
pub(crate) struct PolicyOption {
    value: String,
    label: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "diff_create.html")]
pub(crate) struct CreatePage {
    shell: Shell,
    heading: &'static str,
    action: String,
    /// The revision being updated: its monogram and title.
    revision: Option<(String, String)>,
    /// Whether to show the advice of the first view.
    advice: bool,
    lando_url: String,
    arcanist_href: String,
    error: Option<String>,
    diff: String,
    repository: Option<RepositoryToken>,
    adding: String,
    policies: Vec<PolicyOption>,
    button: &'static str,
    cancel: &'static str,
}

/// The values of the form.
#[derive(Debug, Clone, Default, PartialEq)]
struct Form {
    diff: Option<String>,
    repository_phid: Option<String>,
    view_policy: Option<String>,
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> PageError {
    PageError::Internal(Box::new(error))
}

/// `getStr()`, which turns line endings into `\n`.
fn text(request: &LoginRequest, key: &str) -> Option<String> {
    request
        .str(key)
        .map(|text| text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// `reset($request->getArr(...))`: the first value of the tokenizer, if it
/// has any. Keys given twice, which PHP's arrays merge, are left to PHP.
fn first_token(request: &LoginRequest, key: &str) -> Result<Option<String>, PageError> {
    let entries = request.array(key);
    let mut keys: Vec<&str> = entries
        .iter()
        .map(|(key, _)| key.as_str())
        .filter(|key| !key.is_empty())
        .collect();
    let count = keys.len();
    keys.sort_unstable();
    keys.dedup();
    if keys.len() != count {
        return Err(PageError::Fallback("unusual tokenizer values"));
    }
    Ok(entries.into_iter().next().map(|(_, value)| value))
}

/// The revision `id` of `/differential/revision/update/<id>/`, if the
/// viewer can see and edit it; PHP explains the policy otherwise.
async fn editable_revision<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: &str,
) -> Result<Revision, PageError> {
    let id = crate::revision::parse_monogram(&format!("D{id}")).ok_or(PageError::NotFound)?;
    let database = state.database();
    let as_viewer = viewer.user.viewer();
    let revision = storage::load_revisions_by_id(database, &[id])
        .await?
        .pop()
        .ok_or(PageError::NotFound)?;
    let revision = storage::visible_revisions(
        database,
        state.config(),
        &as_viewer,
        vec![revision],
        state.now(),
    )
    .await
    .map_err(internal)?
    .pop()
    .ok_or(PageError::Fallback("revisions the viewer can't see"))?;
    let can_edit = storage::can_edit(database, state.config(), &as_viewer, &revision, state.now())
        .await
        .map_err(internal)?;
    if !can_edit {
        return Err(PageError::Fallback("revisions the viewer can't edit"));
    }
    Ok(revision)
}

/// The repository of the form, as its handle shows it.
async fn repository_token<S: WebState>(
    state: &S,
    viewer: &WebUser,
    phid: &str,
) -> Result<RepositoryToken, PageError> {
    let database = state.database();
    let found = repository::load(database, &[Phid::new(phid)]).await?;
    let visible = repository::visible(
        database,
        state.config(),
        &viewer.user.viewer(),
        found,
        state.now(),
    )
    .await
    .map_err(internal)?;
    let label = match visible.first() {
        Some(repository) => format!("{} {}", repository.monogram(), repository.name),
        None => "Restricted Repository".to_owned(),
    };
    Ok(RepositoryToken {
        phid: phid.to_owned(),
        label,
    })
}

/// The repository named `typed` (a monogram, callsign, short name, ID or
/// PHID) which the viewer can see.
async fn find_repository<S: WebState>(
    state: &S,
    viewer: &WebUser,
    typed: &str,
) -> Result<Option<String>, PageError> {
    let database = state.database();
    let identifiers = [typed.to_owned()];
    let found: Vec<_> = repository::load_by_identifiers(database, &identifiers)
        .await?
        .into_values()
        .collect();
    let visible = repository::visible(
        database,
        state.config(),
        &viewer.user.viewer(),
        found,
        state.now(),
    )
    .await
    .map_err(internal)?;
    Ok(visible
        .first()
        .map(|repository| repository.phid.as_str().to_owned()))
}

/// The options of the view policy control: the global policies
/// (`PhabricatorPolicyQuery::getGlobalPolicies()`) and the viewer, and the
/// posted value if it is another.
fn policy_options(config: &Config, viewer: &WebUser, selected: &str) -> Vec<PolicyOption> {
    let user = &viewer.user;
    let mut options = Vec::new();
    if config
        .value("policy.allow-public")
        .is_some_and(phab_core::php::is_truthy)
    {
        options.push(("public", "Public (No Login Required)".to_owned()));
    }
    options.push(("users", "All Users".to_owned()));
    options.push(("admin", "Administrators".to_owned()));
    options.push(("no-one", "No One".to_owned()));
    let name = if user.real_name.is_empty() {
        user.user_name.clone()
    } else {
        format!("{} ({})", user.user_name, user.real_name)
    };
    options.push((user.phid.as_str(), format!("Only {name}")));
    let mut options: Vec<PolicyOption> = options
        .into_iter()
        .map(|(value, label)| PolicyOption {
            value: value.to_owned(),
            label,
            selected: value == selected,
        })
        .collect();
    if !options.iter().any(|option| option.selected) {
        options.push(PolicyOption {
            value: selected.to_owned(),
            label: selected.to_owned(),
            selected: true,
        });
    }
    options
}

async fn page<S: WebState>(
    state: &S,
    viewer: &WebUser,
    (revision, uri): (Option<&Revision>, &Uri),
    form: Form,
    (advice, error, adding): (bool, Option<String>, String),
) -> Result<CreatePage, PageError> {
    let repository = match form.repository_phid.as_deref() {
        Some(phid) if !phid.is_empty() => Some(repository_token(state, viewer, phid).await?),
        _ => None,
    };
    // `initializeNewDiff()`: the viewer's own.
    let policy = form
        .view_policy
        .clone()
        .unwrap_or_else(|| viewer.user.phid.as_str().to_owned());
    let (heading, button) = match revision {
        Some(_) => ("Update Diff", "Continue"),
        None => ("Create Diff", "Create Diff"),
    };
    let action = match revision {
        Some(revision) => format!("/differential/revision/update/{}/", revision.id),
        None => "/differential/diff/create/".to_owned(),
    };
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(CreatePage {
        shell: Shell::for_user(state.config(), viewer, heading, current),
        heading,
        action,
        revision: revision.map(|revision| (format!("D{}", revision.id), revision.title.clone())),
        advice,
        lando_url: state
            .config()
            .nonempty_string("lando.url")
            .unwrap_or_default()
            .to_owned(),
        arcanist_href: crate::diffusion::manage::doclink("Arcanist User Guide"),
        error,
        diff: form.diff.unwrap_or_default(),
        repository,
        adding,
        policies: policy_options(state.config(), viewer, &policy),
        button,
        cancel: "/differential/",
    })
}

/// Creates the diff as `differential.createrawdiff` does for the viewer,
/// and returns its ID.
async fn create_diff<S: WebState>(
    state: &S,
    viewer: &WebUser,
    (diff, form): (&str, &Form),
) -> Result<i64, PageError> {
    if state.config().is_read_only() {
        return Err(PageError::Fallback("writes on a read-only install"));
    }
    let context = web_edit::context(state);
    let mut params = Map::new();
    params.insert("diff".to_owned(), json!(diff));
    params.insert("repositoryPHID".to_owned(), json!(form.repository_phid));
    params.insert("viewPolicy".to_owned(), json!(form.view_policy));
    let request = web_edit::request(state, &context, viewer, params);
    let result = match CreateRawDiff.execute(&request).await {
        Ok(result) => result,
        Err(MethodError::Unsupported(reason)) => return Err(PageError::Fallback(reason)),
        // PHP shows its exception, or the form with its validation errors.
        Err(MethodError::Conduit { .. }) => return Err(PageError::Fallback("diffs PHP refuses")),
        Err(MethodError::Internal(error)) => {
            return Err(internal(std::io::Error::other(error)));
        }
    };
    result["id"]
        .as_i64()
        .ok_or_else(|| internal(std::io::Error::other("a diff without an ID")))
}

/// The form, or its post.
async fn respond<S: WebState>(
    state: &S,
    viewer: &WebUser,
    revision: Option<&Revision>,
    request: LoginRequest,
) -> Result<Response, PageError> {
    let uri = request.uri.clone();
    let mut form = Form {
        repository_phid: revision
            .and_then(|revision| revision.repository_phid.as_ref())
            .map(|phid| phid.as_str().to_owned()),
        ..Form::default()
    };
    if !request.is_post() {
        let page = page(
            state,
            viewer,
            (revision, &uri),
            form,
            (true, None, String::new()),
        )
        .await?;
        return Ok(Page(page).into_response());
    }
    web_edit::check_form_post(state, viewer, &request)?;
    if let Some(phid) = first_token(&request, REPOSITORY_KEY)? {
        form.repository_phid = Some(phid);
    }
    form.diff = text(&request, "diff");
    form.view_policy = text(&request, "viewPolicy");
    let typed = request
        .str("rust.add.repository")
        .unwrap_or_default()
        .trim()
        .to_owned();
    if !typed.is_empty() {
        // The Rust form's repository: shown, not written.
        let (error, adding) = match find_repository(state, viewer, &typed).await? {
            Some(phid) => {
                form.repository_phid = Some(phid);
                (None, String::new())
            }
            None => (
                Some(format!("No repository you can see is named {typed}.")),
                typed,
            ),
        };
        let page = page(
            state,
            viewer,
            (revision, &uri),
            form,
            (false, error, adding),
        )
        .await?;
        return Ok(Page(page).into_response());
    }
    let diff = form.diff.clone().unwrap_or_default();
    if diff.is_empty() {
        let page = page(
            state,
            viewer,
            (revision, &uri),
            form,
            (false, Some(EMPTY_DIFF.to_owned()), String::new()),
        )
        .await?;
        return Ok(Page(page).into_response());
    }
    let id = create_diff(state, viewer, (&diff, &form)).await?;
    let path = match revision {
        Some(revision) => format!("/differential/diff/{id}/?revisionID={}", revision.id),
        None => format!("/differential/diff/{id}/"),
    };
    Ok(request.redirect(state.config(), &path))
}

/// `GET|POST /differential/diff/create/`.
pub(crate) async fn create<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    viewer
        .require_application(state, application::DIFFERENTIAL)
        .await?;
    let request = LoginRequest::new(method, uri, headers, &body);
    respond(state, &viewer, None, request).await
}

/// `GET|POST /differential/revision/update/{revisionID}/`.
pub(crate) async fn update<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    viewer
        .require_application(state, application::DIFFERENTIAL)
        .await?;
    let revision = editable_revision(state, &viewer, &id).await?;
    let request = LoginRequest::new(method, uri, headers, &body);
    respond(state, &viewer, Some(&revision), request).await
}

/// PHP's routes of the form.
pub(crate) fn routes<S: WebState>() -> Router<Arc<S>> {
    Router::new()
        .route(
            "/differential/diff/create/",
            routing::get(create::<S>).post(create::<S>),
        )
        .route(
            "/differential/revision/update/{id}/",
            routing::get(update::<S>).post(update::<S>),
        )
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, header};

    use super::*;

    fn post(body: &str) -> LoginRequest {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        LoginRequest::new(
            Method::POST,
            Uri::from_static("/differential/diff/create/"),
            headers,
            &Bytes::from(body.to_owned()),
        )
    }

    #[test]
    fn the_first_repository_token_is_read() {
        let first = |body: &str| first_token(&post(body), REPOSITORY_KEY).ok();
        assert_eq!(first(""), Some(None));
        assert_eq!(
            first("differential%3Arepository%5B%5D=PHID-REPO-a&differential%3Arepository%5B%5D=b"),
            Some(Some("PHID-REPO-a".to_owned()))
        );
        assert_eq!(
            first("differential%3Arepository%5B0%5D="),
            Some(Some(String::new()))
        );
        // PHP's array keeps the last value in the first place.
        assert_eq!(
            first("differential%3Arepository%5B0%5D=a&differential%3Arepository%5B0%5D=b"),
            None
        );
    }
}
