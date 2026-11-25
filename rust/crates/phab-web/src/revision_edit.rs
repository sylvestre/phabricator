//! The edit form of revisions (`/differential/revision/edit/<id>/`,
//! `DifferentialRevisionEditEngine` through
//! `PhabricatorEditEngine::buildEditResponse()`), written by the Rust
//! editor with the edit form's content source ([`Source::EditForm`]).
//!
//! The form posts PHP's fields, so the replay of a post Rust leaves to PHP
//! is PHP's own form post: `title`, `author`, `summary` and `testPlan`
//! (with their `_metadata`), `reviewers`, `repositoryPHID`, `policy.view`,
//! `policy.edit`, `projectPHIDs`, `subscriberPHIDs` (the tokenizers with
//! their `.initial` values) and the Bugzilla bug ID
//! (`differential:bugzilla-bug-id`). PHP turns every field into a
//! transaction, in the order of its fields (the engine's, then the policy,
//! project, subscription and custom field extensions), and its editor
//! continues on no effect; Rust sends the editor the fields which change,
//! as PHP's would keep them, and always the custom fields, which PHP
//! validates even without effect: the bug ID, and the uplift request,
//! which has no control and which PHP reads as `[]`.
//!
//! Left to PHP (`421`, before writing): changes to the author, the
//! repository or the policies, a missing field, attached files, workflow
//! (Ajax) requests, invalid CSRF tokens, viewers who can't edit the
//! revision, configured revision forms or `differential.fields` which add
//! or remove fields of the form, revisions with an uplift request (which
//! PHP's form clears), validation errors PHP shows on the form (empty
//! titles and test plans, texts which read as commit message fields), list
//! changes which both add and remove, edits without effect, and every edit
//! the editor leaves to PHP.
//!
//! Without JavaScript tokenizers, the Rust form lists the tokens as
//! checkboxes and adds names typed in its own `rust.add.*` inputs: a post
//! with such names doesn't write, it shows the form again with their tokens
//! checked, so that the post which writes is a PHP form post.
//!
//! The same form updates a revision with a diff which has none
//! (`/differential/revision/attach/<diffID>/to/<id>/`,
//! `DifferentialRevisionEditController` giving the engine the diff): PHP
//! adds the diff's invisible `update[]` field and the `update.comment`
//! comment, whose transactions come first; an empty comment has no effect
//! but subscribes the actor, as a comment does.
//! Left to PHP there too: diffs already attached (PHP's dialog), posts
//! without `update[]` (PHP shows the form again) or with another diff,
//! comments of whitespace (which the editor leaves to PHP), and new revisions
//! (`/differential/revision/attach/<diffID>/to/`), whose policies PHP's
//! form sets, which the Rust editor doesn't on creation.

use std::sync::Arc;

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Router, routing};
use phab_app_differential::edit::{self, Source};
use phab_app_differential::storage::{self, Revision};
use phab_core::{Config, Phid, php};
use phab_db::edges;
use phab_policy::application;
use serde_json::{Value, json};

use crate::error::PageError;
use crate::harbormaster::{self, Scope};
use crate::login::request::LoginRequest;
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::viewer::WebUser;
use crate::web_edit::{self, FormAction};
use crate::{actions, diff_page};

/// `PhabricatorCookies::COOKIE_SUBMIT`.
const SUBMIT_COOKIE: &str = "phfrm";
/// `getEditEngineSubmitKey()`.
const SUBMIT_KEY: &str = "edit-engine/differential.revision";
/// The Bugzilla bug ID's field key, which is its input's name.
const BUG_ID: &str = "differential:bugzilla-bug-id";
/// The uplift request's field key.
const UPLIFT: &str = "differential:uplift-request";

/// The tokenizers of the form: their key, the Conduit edit type of their
/// transactions, their label and whether they take only projects.
const LISTS: [(&str, &str, &str, bool); 3] = [
    ("reviewers", "reviewers", "Reviewers", false),
    ("projectPHIDs", "projects", "Tags", true),
    ("subscriberPHIDs", "subscribers", "Subscribers", false),
];

/// A token of a tokenizer.
pub(crate) struct Token {
    value: String,
    label: String,
}

/// A tokenizer, as checkboxes.
pub(crate) struct TokenField {
    key: &'static str,
    label: &'static str,
    tokens: Vec<Token>,
    initial: Vec<String>,
    /// What was typed in its `rust.add.*` input, kept after an error.
    adding: String,
}

/// What the form of an update with a diff adds.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Update {
    diff_id: i64,
    diff_phid: String,
    /// `update.comment`.
    comment: String,
}

#[derive(Template)]
#[template(path = "revision_edit.html")]
pub(crate) struct EditPage {
    shell: Shell,
    id: i64,
    action: String,
    heading: String,
    error: Option<String>,
    update: Option<Update>,
    form: Form,
    lists: Vec<TokenField>,
}

/// The values of the form's fields, as posted or as the revision has them.
#[derive(Debug, Clone, Default, PartialEq)]
struct Form {
    title: Option<String>,
    author: Vec<String>,
    summary: Option<String>,
    summary_metadata: Option<String>,
    test_plan: Option<String>,
    test_plan_metadata: Option<String>,
    repository: Vec<String>,
    view_policy: Option<String>,
    edit_policy: Option<String>,
    /// The tokenizers of [`LISTS`]: their values and initial values.
    lists: [(Vec<String>, Vec<String>); 3],
    bug_id: Option<String>,
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

/// `getStrList()`: the items between whitespace and commas.
fn str_list(text: &str) -> Vec<String> {
    text.split(|character: char| {
        matches!(character, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r' | ',')
    })
    .filter(|item| !item.is_empty())
    .map(str::to_owned)
    .collect()
}

/// `AphrontStringListHTTPParameterType`: `getArr()`, else `getStrList()`;
/// a missing tokenizer is an empty one.
fn list(request: &LoginRequest, key: &str) -> Vec<String> {
    let array = request.array(key);
    if !array.is_empty() {
        return array.into_iter().map(|(_, value)| value).collect();
    }
    request.str(key).map(str_list).unwrap_or_default()
}

impl Form {
    /// The form of the revision: what PHP's form starts with.
    async fn of_revision<S: WebState>(
        state: &S,
        revision: &Revision,
        stored: &Stored,
    ) -> Result<Self, PageError> {
        let database = state.database();
        let details = phab_app_differential::details::load_details(database, &revision.phid)
            .await?
            .ok_or(PageError::NotFound)?;
        let phids = std::slice::from_ref(&revision.phid);
        // getReviewerPHIDsForEdit().
        let reviewers: Vec<String> = revision
            .reviewers
            .iter()
            .map(|reviewer| {
                if reviewer.status == "blocking" {
                    format!("blocking({})", reviewer.phid)
                } else {
                    reviewer.phid.to_string()
                }
            })
            .collect();
        let mut projects: Vec<String> =
            edges::load_destination_phids(database, phids, &[edges::OBJECT_HAS_PROJECT], None)
                .await?
                .iter()
                .map(ToString::to_string)
                .collect();
        projects.reverse();
        let subscribers: Vec<String> =
            edges::load_destination_phids(database, phids, &[edges::OBJECT_HAS_SUBSCRIBER], None)
                .await?
                .iter()
                .map(ToString::to_string)
                .collect();
        Ok(Self {
            title: Some(revision.title.clone()),
            author: revision
                .author_phid
                .iter()
                .map(ToString::to_string)
                .collect(),
            summary: Some(details.summary),
            summary_metadata: Some("{}".to_owned()),
            test_plan: Some(details.test_plan),
            test_plan_metadata: Some("{}".to_owned()),
            repository: revision
                .repository_phid
                .iter()
                .map(ToString::to_string)
                .collect(),
            view_policy: Some(revision.view_policy.clone()),
            edit_policy: Some(revision.edit_policy.clone()),
            lists: [
                (reviewers.clone(), reviewers),
                (projects.clone(), projects),
                (subscribers.clone(), subscribers),
            ],
            bug_id: Some(stored.bug_id.clone().unwrap_or_default()),
        })
    }

    /// The form as posted.
    fn from_request(request: &LoginRequest) -> Self {
        let list_field = |key: &str| {
            let initial = request
                .array(&format!("{key}.initial"))
                .into_iter()
                .map(|(_, value)| value)
                .collect();
            (list(request, key), initial)
        };
        Self {
            title: text(request, "title"),
            author: list(request, "author"),
            summary: text(request, "summary"),
            summary_metadata: request.str("summary_metadata").map(str::to_owned),
            test_plan: text(request, "testPlan"),
            test_plan_metadata: request.str("testPlan_metadata").map(str::to_owned),
            repository: list(request, "repositoryPHID"),
            view_policy: request.str("policy.view").map(str::to_owned),
            edit_policy: request.str("policy.edit").map(str::to_owned),
            lists: LISTS.map(|(key, ..)| list_field(key)),
            bug_id: text(request, BUG_ID),
        }
    }
}

/// The stored custom fields of the revision.
struct Stored {
    bug_id: Option<String>,
    uplift: Option<String>,
}

impl Stored {
    async fn load<S: WebState>(state: &S, revision: &Revision) -> Result<Self, PageError> {
        let database = state.database();
        let sql = format!(
            "SELECT fieldIndex, fieldValue FROM {} WHERE objectPHID = ?",
            database.table("differential", "differential_customfieldstorage")
        );
        let rows = phab_db::sqlx::query(&sql)
            .bind(revision.phid.as_str())
            .fetch_all(database.pool())
            .await
            .map_err(phab_db::DbError::from)?;
        let index = |key: &str| phab_core::hash::digest_for_index(key.as_bytes());
        let (bug_index, uplift_index) = (index(BUG_ID), index(UPLIFT));
        let mut stored = Self {
            bug_id: None,
            uplift: None,
        };
        for row in &rows {
            let key = phab_db::rows::text(row, "fieldIndex")?;
            let value = phab_db::rows::text(row, "fieldValue")?;
            if key == bug_index {
                stored.bug_id = Some(value);
            } else if key == uplift_index {
                stored.uplift = Some(value);
            }
        }
        Ok(stored)
    }

    /// Whether the revision has an uplift request, which PHP's form, which
    /// reads none, clears.
    fn has_uplift(&self) -> bool {
        self.uplift
            .as_deref()
            .and_then(|value| serde_json::from_str::<Value>(value).ok())
            .is_some_and(|value| match value {
                Value::Array(list) => !list.is_empty(),
                Value::Object(map) => !map.is_empty(),
                Value::Null => false,
                _ => true,
            })
    }
}

/// The custom fields of the form, in the order PHP builds them
/// (`buildFieldList()`): those `differential.fields` lists first; why the
/// form has other fields than the ported ones otherwise.
fn custom_field_order(config: &Config) -> Result<[&'static str; 2], &'static str> {
    let spec = match config.value("differential.fields") {
        None => return Ok([BUG_ID, UPLIFT]),
        Some(Value::Object(spec)) => spec,
        Some(_) => return Err("an unusual differential.fields"),
    };
    // `isset($spec[$key]['disabled'])`, else `shouldDisableByDefault()`.
    let disabled = |key: &str, default: bool| {
        spec.get(key)
            .and_then(Value::as_object)
            .and_then(|entry| entry.get("disabled"))
            .filter(|value| !value.is_null())
            .map_or(default, php::is_truthy)
    };
    if disabled("differential:test-plan", false) {
        return Err("revision forms without a test plan");
    }
    if !disabled("phabricator:blame-revision", true) || !disabled("phabricator:revert-plan", true) {
        return Err("revision forms with more fields");
    }
    let position = |key: &str| spec.keys().position(|listed| listed == key);
    let mut order = [BUG_ID, UPLIFT];
    order.sort_by_key(|key| position(key).unwrap_or(usize::MAX));
    Ok(order)
}

/// The revision `id`, if the viewer can see and edit it, and its form is
/// the ported one.
async fn editable<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: &str,
) -> Result<Revision, PageError> {
    viewer
        .require_application(state, application::DIFFERENTIAL)
        .await?;
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
    .ok_or(PageError::NotFound)?;
    let can_edit = storage::can_edit(database, state.config(), &as_viewer, &revision, state.now())
        .await
        .map_err(internal)?;
    if !can_edit {
        // PHP explains the policy.
        return Err(PageError::Fallback("revisions the viewer can't edit"));
    }
    if web_edit::has_configured_forms(state).await? {
        return Err(PageError::Fallback("configured revision forms"));
    }
    custom_field_order(state.config()).map_err(PageError::Fallback)?;
    Ok(revision)
}

/// Whether remarkup metadata is the empty one of the form, which files
/// dropped on the text area fill: `{}` decodes to an empty list.
fn is_empty_metadata(metadata: Option<&str>) -> bool {
    match metadata {
        None => true,
        Some(text) => {
            matches!(
                serde_json::from_str::<Value>(text),
                Ok(Value::Object(map)) if map.is_empty()
            ) || matches!(serde_json::from_str::<Value>(text), Ok(Value::Array(list)) if list.is_empty())
        }
    }
}

/// `formatBugID()`: `trim(str_replace('#', '', $id))`.
fn format_bug_id(value: &str) -> String {
    value
        .replace('#', "")
        .trim_matches([' ', '\t', '\n', '\r', '\0', '\x0B'])
        .to_owned()
}

/// The edge transaction of a tokenizer whose value changes the current
/// list, as `PhabricatorPHIDListEditField` makes it from the posted value
/// and initial value; `None` when it has no effect.
fn list_change(
    edit_type: &str,
    (value, initial): &(Vec<String>, Vec<String>),
    current: &[String],
) -> Result<Option<Value>, &'static str> {
    let action = FormAction {
        kind: edit_type.to_owned(),
        value: json!(value),
        initial: json!(initial),
    };
    let transaction = match web_edit::list_transaction(edit_type, &action) {
        Ok(transaction) => transaction,
        Err("list changes without effect") => return Ok(None),
        Err(reason) => return Err(reason),
    };
    let items: Vec<&str> = transaction["value"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let is_add = transaction["type"].as_str().is_some_and(|kind| {
        kind.split_once('.')
            .is_some_and(|(_, operation)| operation == "add")
    });
    let changes = if is_add {
        items
            .iter()
            .any(|item| !current.iter().any(|known| known == item))
    } else {
        items.iter().any(|item| {
            current
                .iter()
                .any(|known| actions::token_phid(known) == actions::token_phid(item))
        })
    };
    Ok(changes.then_some(transaction))
}

/// The editor's transactions for `form` on `revision`, in PHP's order, or
/// why PHP must answer: with `update`, the diff and the comment first.
fn transactions(
    config: &Config,
    (current, form): (&Form, &Form),
    stored: &Stored,
    update: Option<&Update>,
) -> Result<Vec<Value>, PageError> {
    let fallback = PageError::Fallback;
    let (Some(title), Some(summary), Some(test_plan)) =
        (&form.title, &form.summary, &form.test_plan)
    else {
        return Err(fallback("form posts without every field"));
    };
    if form.author != current.author
        || form.repository != current.repository
        || form.view_policy != current.view_policy
        || form.edit_policy != current.edit_policy
    {
        return Err(fallback("changes to the author, repository or policies"));
    }
    if !is_empty_metadata(form.summary_metadata.as_deref())
        || !is_empty_metadata(form.test_plan_metadata.as_deref())
    {
        return Err(fallback("texts with attached files"));
    }
    if stored.has_uplift() {
        return Err(fallback("revisions with an uplift request"));
    }
    // What PHP validates even without effect.
    if title.is_empty() {
        return Err(fallback("empty titles"));
    }
    let required = config
        .value("differential.require-test-plan-field")
        .is_none_or(php::is_truthy);
    if required && test_plan.is_empty() {
        return Err(fallback("empty test plans"));
    }
    for text in [summary, test_plan] {
        let names = edit::names_commit_message_fields(config, text)
            .map_err(|_| fallback("texts with carriage returns"))?;
        if names {
            return Err(fallback("texts which read as commit message fields"));
        }
    }

    let mut transactions = Vec::new();
    if let Some(update) = update {
        // The comment field always makes a transaction: an empty one
        // still subscribes the actor, then has no effect.
        transactions.push(json!({"type": "update", "value": update.diff_phid}));
        transactions.push(json!({"type": "comment", "value": update.comment}));
    }
    if Some(title) != current.title.as_ref() {
        transactions.push(json!({"type": "title", "value": title}));
    }
    if Some(summary) != current.summary.as_ref() {
        transactions.push(json!({"type": "summary", "value": summary}));
    }
    if Some(test_plan) != current.test_plan.as_ref() {
        transactions.push(json!({"type": "testPlan", "value": test_plan}));
    }
    for ((_, edit_type, ..), (posted, (current, _))) in
        LISTS.iter().zip(form.lists.iter().zip(&current.lists))
    {
        if let Some(transaction) = list_change(edit_type, posted, current).map_err(fallback)? {
            transactions.push(transaction);
        }
    }
    let bug_id = format_bug_id(form.bug_id.as_deref().unwrap_or_default());
    // An update always has an effect.
    if transactions.is_empty() && stored.bug_id.as_deref() == Some(bug_id.as_str()) {
        // PHP still saves and publishes the revision.
        return Err(fallback("edits without effect"));
    }
    let order = custom_field_order(config).map_err(fallback)?;
    for key in order {
        transactions.push(if key == BUG_ID {
            json!({"type": "bugzilla.bug-id", "value": bug_id})
        } else {
            json!({"type": "uplift.request", "value": "[]"})
        });
    }
    Ok(transactions)
}

/// Adds the names typed in the `rust.add.*` inputs to the tokenizers.
async fn add_names<S: WebState>(
    state: &S,
    viewer: &WebUser,
    form: &mut Form,
    typed: &[String; 3],
) -> Result<(), PageError> {
    for (index, ((_, _, label, projects_only), (typed, (values, _)))) in LISTS
        .iter()
        .zip(typed.iter().zip(form.lists.iter_mut()))
        .enumerate()
    {
        // Only reviewers block.
        let blocks = index == 0;
        let names = actions::names(typed);
        if *projects_only && let Some(name) = names.iter().find(|name| !name.is_project) {
            return Err(PageError::Invalid(format!(
                "{label} are projects: {} is not a #hashtag.",
                name.name
            )));
        }
        for (phid, is_blocking) in actions::resolve(state, viewer, &names).await? {
            let token = if is_blocking && blocks {
                format!("blocking({phid})")
            } else {
                phid.to_string()
            };
            match values
                .iter_mut()
                .find(|known| actions::token_phid(known) == phid.as_str())
            {
                Some(known) => *known = token,
                None => values.push(token),
            }
        }
    }
    Ok(())
}

async fn page<S: WebState>(
    state: &S,
    viewer: &WebUser,
    (revision, uri, update): (&Revision, &Uri, Option<Update>),
    form: Form,
    (error, typed): (Option<String>, [String; 3]),
) -> Result<EditPage, PageError> {
    let phids: Vec<Phid> = form
        .lists
        .iter()
        .flat_map(|(values, _)| values)
        .map(|token| Phid::new(actions::token_phid(token)))
        .collect();
    let scope = Scope::new(state, viewer);
    let handles = harbormaster::load_handles(state, &scope, &phids).await?;
    let lists = LISTS
        .iter()
        .zip(form.lists.iter())
        .zip(typed)
        .map(
            |(((key, _, label, _), (values, initial)), adding)| TokenField {
                key,
                label,
                tokens: values
                    .iter()
                    .map(|value| {
                        let phid = Phid::new(actions::token_phid(value));
                        let name = harbormaster::handle(&handles, &phid).full_name.clone();
                        let label = if value.starts_with("blocking(") {
                            format!("{name} (blocking)")
                        } else {
                            name
                        };
                        Token {
                            value: value.clone(),
                            label,
                        }
                    })
                    .collect(),
                initial: initial.clone(),
                adding,
            },
        )
        .collect();
    // `getObjectEditTitleText()`.
    let (verb, action) = match &update {
        Some(update) => (
            "Update",
            format!(
                "/differential/revision/attach/{}/to/{}/",
                update.diff_id, revision.id
            ),
        ),
        None => (
            "Edit",
            format!("/differential/revision/edit/{}/", revision.id),
        ),
    };
    let heading = format!("{verb} Revision D{}: {}", revision.id, revision.title);
    let current = uri.path_and_query().map_or("/", |path| path.as_str());
    Ok(EditPage {
        shell: Shell::for_user(state.config(), viewer, &heading, current),
        id: revision.id,
        action,
        heading,
        error,
        update,
        form,
        lists,
    })
}

/// The update of a post of the form with a diff: the posted `update[]`
/// must be the diff (PHP shows the form again without it).
fn posted_update(
    request: &LoginRequest,
    update: Option<&Update>,
) -> Result<Option<Update>, PageError> {
    let Some(update) = update else {
        return Ok(None);
    };
    let exists = request.exists("update") || !request.array("update").is_empty();
    if !exists {
        return Err(PageError::Fallback("updates without the diff"));
    }
    if list(request, "update") != [update.diff_phid.as_str()] {
        return Err(PageError::Fallback("updates with another diff"));
    }
    Ok(Some(Update {
        comment: text(request, "update.comment").unwrap_or_default(),
        ..update.clone()
    }))
}

/// The form of `revision`, or its post; with `update`, the form which
/// updates it with a diff.
async fn respond<S: WebState>(
    state: &S,
    viewer: &WebUser,
    (revision, update): (Revision, Option<Update>),
    request: LoginRequest,
) -> Result<Response, PageError> {
    let uri = request.uri.clone();
    let stored = Stored::load(state, &revision).await?;
    let current = Form::of_revision(state, &revision, &stored).await?;
    if !request.is_post() {
        let page = page(
            state,
            viewer,
            (&revision, &uri, update),
            current,
            (None, Default::default()),
        )
        .await?;
        return Ok(Page(page).into_response());
    }
    web_edit::check_form_post(state, viewer, &request)?;
    if !request.bool("editEngine") {
        // PHP shows the form again.
        return Err(PageError::Fallback("posts which aren't edit form posts"));
    }
    let update = posted_update(&request, update.as_ref())?;
    let mut form = Form::from_request(&request);
    let typed = LISTS.map(|(key, ..)| {
        request
            .str(&format!("rust.add.{key}"))
            .unwrap_or_default()
            .to_owned()
    });
    if typed.iter().any(|typed| !typed.trim().is_empty()) {
        // The Rust form's additions: shown, not written.
        let (error, typed) = match add_names(state, viewer, &mut form, &typed).await {
            Ok(()) => (None, Default::default()),
            Err(PageError::Invalid(message)) => (Some(message), typed),
            Err(error) => return Err(error),
        };
        let page = page(
            state,
            viewer,
            (&revision, &uri, update),
            form,
            (error, typed),
        )
        .await?;
        return Ok(Page(page).into_response());
    }
    let transactions = transactions(state.config(), (&current, &form), &stored, update.as_ref())?;
    web_edit::apply(
        state,
        viewer,
        (revision.id, transactions),
        Source::EditForm,
        false,
    )
    .await?;
    // `newEditResponse()`.
    let mut request = request;
    request.set_temporary_cookie(state.config(), SUBMIT_COOKIE, SUBMIT_KEY, state.now());
    Ok(request.redirect(state.config(), &format!("/D{}", revision.id)))
}

/// `GET|POST /differential/revision/edit/{id}/`.
pub(crate) async fn edit_form<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    // `?diffID=` and the like belong to other workflows.
    if uri.query().is_some_and(|query| !query.is_empty()) {
        return Err(PageError::Fallback("edit forms with parameters"));
    }
    let revision = editable(state, &viewer, &id).await?;
    let request = LoginRequest::new(method, uri, headers, &body);
    respond(state, &viewer, (revision, None), request).await
}

/// The diff `id` of the attach route, which PHP loads for the viewer
/// before the revision: an update needs one without a revision.
async fn attachable_diff<S: WebState>(
    state: &S,
    viewer: &WebUser,
    id: &str,
) -> Result<Update, PageError> {
    viewer
        .require_application(state, application::DIFFERENTIAL)
        .await?;
    // `withIDs(array($id))`, which MySQL compares as a number.
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PageError::Fallback("unusual diff IDs"));
    }
    let id = id
        .parse()
        .map_err(|_| PageError::Fallback("unusual diff IDs"))?;
    let diff = diff_page::load_diff(state, viewer, id).await?;
    if diff.revision_id.is_some_and(|id| id != 0) {
        // PHP's "Diff Already Attached" dialog.
        return Err(PageError::Fallback("diffs already attached"));
    }
    Ok(Update {
        diff_id: diff.id,
        diff_phid: diff.phid.as_str().to_owned(),
        comment: String::new(),
    })
}

/// `GET|POST /differential/revision/attach/{diffID}/to/{id}/`: the form
/// which updates revision `id` with the diff.
pub(crate) async fn attach_form<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path((diff_id, id)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let state = state.as_ref();
    if uri.query().is_some_and(|query| !query.is_empty()) {
        return Err(PageError::Fallback("edit forms with parameters"));
    }
    let update = attachable_diff(state, &viewer, &diff_id).await?;
    // `(?P<id>[0-9]\d*)`; anything else is an edit action of a new
    // revision.
    if crate::revision::parse_monogram(&format!("D{id}")).is_none() {
        return Err(PageError::Fallback("new revisions from a diff"));
    }
    let revision = editable(state, &viewer, &id).await?;
    let request = LoginRequest::new(method, uri, headers, &body);
    respond(state, &viewer, (revision, Some(update)), request).await
}

/// PHP's routes of the edit form.
pub(crate) fn routes<S: WebState>() -> Router<Arc<S>> {
    let new_revision = || async { crate::fallback_response("new revisions from a diff") };
    let edit_action = || async { crate::fallback_response("edit actions of revision forms") };
    Router::new()
        .route(
            "/differential/revision/edit/{id}/",
            routing::get(edit_form::<S>).post(edit_form::<S>),
        )
        .route(
            "/differential/revision/attach/{diff}/to/",
            routing::any(new_revision),
        )
        .route(
            "/differential/revision/attach/{diff}/to/{id}/",
            routing::get(attach_form::<S>).post(attach_form::<S>),
        )
        .route(
            "/differential/revision/attach/{diff}/to/{id}/{action}/",
            routing::any(edit_action),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(fields: Option<Value>) -> Config {
        let mut config = Config::default();
        let mut values = serde_json::Map::new();
        if let Some(fields) = fields {
            values.insert("differential.fields".to_owned(), fields);
        }
        config.push_layer("test", values);
        config
    }

    #[test]
    fn custom_fields_follow_differential_fields() {
        assert_eq!(custom_field_order(&config(None)), Ok([BUG_ID, UPLIFT]));
        assert_eq!(
            custom_field_order(&config(Some(json!({UPLIFT: {}, BUG_ID: {}})))),
            Ok([UPLIFT, BUG_ID])
        );
        assert!(
            custom_field_order(&config(Some(
                json!({"differential:test-plan": {"disabled": true}})
            )))
            .is_err()
        );
        assert!(
            custom_field_order(&config(Some(
                json!({"phabricator:revert-plan": {"disabled": false}})
            )))
            .is_err()
        );
    }

    #[test]
    fn bug_ids_are_formatted_like_php() {
        assert_eq!(format_bug_id(" #1234 \n"), "1234");
        assert_eq!(format_bug_id(""), "");
    }

    #[test]
    fn metadata_must_be_empty() {
        assert!(is_empty_metadata(None));
        assert!(is_empty_metadata(Some("{}")));
        assert!(is_empty_metadata(Some("[]")));
        assert!(!is_empty_metadata(Some(r#"{"attachedFilePHIDs":[]}"#)));
        assert!(!is_empty_metadata(Some("nope")));
    }

    #[test]
    fn lists_change_from_their_initial_value() {
        let lists = |value: &[&str], initial: &[&str]| {
            (
                value.iter().map(ToString::to_string).collect::<Vec<_>>(),
                initial.iter().map(ToString::to_string).collect::<Vec<_>>(),
            )
        };
        let current = vec!["PHID-USER-a".to_owned()];
        assert_eq!(
            list_change(
                "reviewers",
                &lists(&["PHID-USER-a", "PHID-USER-b"], &["PHID-USER-a"]),
                &current
            ),
            Ok(Some(
                json!({"type": "reviewers.add", "value": ["PHID-USER-b"]})
            ))
        );
        assert_eq!(
            list_change("reviewers", &lists(&[], &["PHID-USER-a"]), &current),
            Ok(Some(
                json!({"type": "reviewers.remove", "value": ["PHID-USER-a"]})
            ))
        );
        // Unchanged, or adding what is there already: no effect.
        assert_eq!(
            list_change(
                "reviewers",
                &lists(&["PHID-USER-a"], &["PHID-USER-a"]),
                &current
            ),
            Ok(None)
        );
        assert_eq!(
            list_change("reviewers", &lists(&["PHID-USER-a"], &[]), &current),
            Ok(None)
        );
        assert!(
            list_change(
                "reviewers",
                &lists(&["blocking(PHID-USER-a)"], &["PHID-USER-a"]),
                &current
            )
            .is_err()
        );
    }

    #[test]
    fn lists_read_arrays_or_strings() {
        assert_eq!(str_list("a, b,,c"), ["a", "b", "c"]);
    }
}
