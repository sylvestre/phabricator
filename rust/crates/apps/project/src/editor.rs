//! `PhabricatorProjectTransactionEditor`: what `project.edit` and
//! `project.create` write.
//!
//! An edit is planned first: the checks PHP's editor makes, the old and new
//! values of the transactions, Herald and the publish task. Anything not
//! ported is [`MethodError::Unsupported`] there, before anything is
//! written. Then one MySQL transaction writes the rows PHP's editor does:
//! the project, its datasource tokens, the transactions, the hashtags, the
//! member edges and their materialization, the Herald transcript and the
//! `PhabricatorApplicationTransactionPublishWorker` task, so mail, feed
//! stories and search indexing still come from PHP's daemons.
//!
//! Ported transactions: the name, icon, color, additional hashtags,
//! members and watchers of root projects, which are neither milestones,
//! nor parents, nor archived. Edits come from Conduit or from the web
//! forms and actions ([`Source`]), which only differ by their content
//! source. Left to PHP: everything else, with custom fields (the
//! description), spaces, Herald rules about projects, transactions
//! without effect on an existing project when none has one, names and
//! hashtags which are not printable ASCII (PHP lowercases them with
//! `mbstring`), and validation errors whose message depends on the
//! translation (hashtags) or which PHP drops (empty names on edits).

use phab_app_herald::engine::{self, HeraldError, Subject};
use phab_conduit::{ApiRequest, MethodError, pager};
use phab_core::{Phid, PhidType, hash, php, random};
use phab_db::editor::{self as db_editor, CONDUIT_SOURCE, TransactionRow};
use phab_db::sqlx::MySqlConnection;
use phab_db::{Database, edges, rows, worker};
use phab_policy::Capability;
use serde_json::{Map, Value, json};

use crate::policy;
use crate::search::{self, IconSet};
use crate::storage::Project;

/// `PhabricatorProjectMaterializedMemberEdgeType`.
const MATERIALIZED_MEMBER: i64 = edges::PROJECT_MATERIALIZED_MEMBER;
/// `PhabricatorProjectApplication`.
pub(crate) const APPLICATION_CLASS: &str = "PhabricatorProjectApplication";
/// `PhabricatorProjectHeraldAdapter::getAdapterContentType()`.
const HERALD_CONTENT_TYPE: &str = "PhabricatorProjectHeraldAdapter";
/// The byte length of the `name` column (`sort128`).
const NAME_LENGTH: usize = 128;

/// `PhabricatorObjectHasWatcherEdgeType`.
const OBJECT_HAS_WATCHER: i64 = crate::storage::OBJECT_HAS_WATCHER;
/// `PhabricatorWatcherHasObjectEdgeType`.
const WATCHER_HAS_OBJECT: i64 = 48;
/// The content source of web requests (`PhabricatorWebContentSource`).
const WEB_SOURCE: &str = r#"{"source":"web","params":[]}"#;

/// Where an edit comes from (`setContentSource()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `project.edit` and `project.create`.
    Conduit,
    /// The web forms and actions (`setContentSourceFromRequest()`).
    Web,
}

impl Source {
    /// The serialized content source of the transactions.
    fn content_source(self) -> &'static str {
        match self {
            Self::Conduit => CONDUIT_SOURCE,
            Self::Web => WEB_SOURCE,
        }
    }

    /// The `via` mail stamp.
    fn via(self) -> &'static str {
        match self {
            Self::Conduit => "conduit",
            Self::Web => "web",
        }
    }
}

/// What a list transaction does (`+`, `-` or `=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// `members.add`.
    Add,
    /// `members.remove`.
    Remove,
    /// `members.set`.
    Set,
}

/// One transaction of the request, as the edit engine, `project.create`
/// or a web action builds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// `core:create`.
    Create,
    /// `project:name`.
    Name(String),
    /// `project:icon`.
    Icon(String),
    /// `project:color`.
    Color(String),
    /// `project:slugs`, as given.
    Slugs(Vec<String>),
    /// The member edge transaction.
    Members(Operation, Vec<String>),
    /// The watcher edge transaction (`PhabricatorProjectWatchController`).
    Watchers(Operation, Vec<String>),
}

impl Change {
    /// The edge type of an edge transaction, and its inverse.
    fn edge_types(&self) -> Option<(i64, i64)> {
        match self {
            Self::Members(..) => Some((edges::PROJECT_HAS_MEMBER, edges::MEMBER_OF_PROJECT)),
            Self::Watchers(..) => Some((OBJECT_HAS_WATCHER, WATCHER_HAS_OBJECT)),
            _ => None,
        }
    }
}

/// The project an edit applies to: `None` creates one.
pub(crate) struct Target {
    pub(crate) project: Option<Project>,
}

/// What the methods answer with, once the edit is written.
pub(crate) struct Outcome {
    pub(crate) id: i64,
    pub(crate) phid: String,
    pub(crate) name: String,
    pub(crate) icon: String,
    pub(crate) color: String,
    pub(crate) transactions: Vec<String>,
}

/// What a project row needs beyond [`Project`], for edits.
struct Extra {
    is_membership_locked: bool,
    has_subprojects: bool,
}

/// The edge lists of a project before the edit.
#[derive(Default)]
struct Lists {
    members: Vec<String>,
    watchers: Vec<String>,
}

/// A transaction once its values are known.
struct Planned {
    phid: String,
    kind: &'static str,
    old: Value,
    new: Value,
    /// For edges: the removed and the added members.
    edge: Option<(Vec<String>, Vec<String>)>,
    change: Change,
}

/// The project as the edit leaves it.
struct State {
    id: Option<i64>,
    phid: String,
    name: String,
    icon: String,
    color: String,
    primary_slug: Option<String>,
    /// Hashtags, as `updateDatasourceTokens()` reads them when the project
    /// is saved: before this edit's hashtags.
    slugs: Vec<String>,
    view_policy: String,
    edit_policy: String,
    join_policy: String,
}

/// The planned edit.
struct Plan {
    actor: String,
    source: Source,
    state: State,
    transactions: Vec<Planned>,
    group_id: String,
    transcript: engine::TranscriptRow,
    herald_header: String,
    worker_data: String,
}

fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// Text PHP and Rust lowercase and split alike: printable ASCII.
fn is_plain(text: &str) -> bool {
    text.bytes().all(|byte| (b' '..=b'~').contains(&byte))
}

/// `PhabricatorSlug::normalizeProjectSlug()` of plain text.
fn normalize_slug(slug: &str) -> String {
    search::valid_slug(slug).unwrap_or_else(|| "_".to_owned())
}

/// `PhabricatorProjectTransactionEditor::normalizeSlugs()`.
pub(crate) fn normalize_slugs(slugs: &[String]) -> Vec<String> {
    let mut normal: Vec<String> = Vec::new();
    for slug in slugs {
        let slug = normalize_slug(slug);
        if !normal.contains(&slug) {
            normal.push(slug);
        }
    }
    normal
}

/// `PhabricatorTypeaheadDatasource::tokenizeString()` of plain text.
fn tokenize(text: &str) -> Vec<String> {
    let lowered = text.to_ascii_lowercase();
    let mut tokens: Vec<String> = Vec::new();
    for token in lowered
        .trim_matches([' ', '\t', '\n', '\r', '\0', '\x0b'])
        .split(|character: char| character.is_ascii_whitespace() || "[]()-\x0b".contains(character))
    {
        if !token.is_empty() && !tokens.iter().any(|known| known == token) {
            tokens.push(token.to_owned());
        }
    }
    tokens
}

/// `getHeraldRuleMonograms()`: `<1>, <2>` is `["H1", "H2"]`; PHP unsets
/// what is not a rule, so a list with holes is a JSON object.
fn herald_monograms(header: &str) -> Value {
    let mut map = Map::new();
    let mut holes = false;
    for (index, item) in header
        .split([',', ' '])
        .filter(|part| !part.is_empty())
        .enumerate()
    {
        let item = item.trim_matches(['<', '>']);
        if item.is_empty() || !item.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        holes |= index != map.len();
        map.insert(index.to_string(), json!(format!("H{item}")));
    }
    if holes {
        Value::Object(map)
    } else {
        Value::Array(map.into_iter().map(|(_, value)| value).collect())
    }
}

/// `getPHIDList()`/`getEdgeTransactionNewValue()`: the old PHIDs a set
/// keeps, the set's, the added ones, less the removed ones.
fn new_list(old: &[String], operation: Operation, phids: &[String]) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    for phid in old {
        if (operation != Operation::Set || phids.contains(phid)) && !result.contains(phid) {
            result.push(phid.clone());
        }
    }
    match operation {
        Operation::Remove => result.retain(|phid| !phids.contains(phid)),
        Operation::Add | Operation::Set => {
            for phid in phids {
                if !result.contains(phid) {
                    result.push(phid.clone());
                }
            }
        }
    }
    result
}

/// The items of `from` which are not in `other`, in order.
fn minus(from: &[String], other: &[String]) -> Vec<String> {
    from.iter()
        .filter(|item| !other.contains(item))
        .cloned()
        .collect()
}

/// Whether anything installed changes what the editor does in ways not
/// ported: custom fields, spaces, Herald rules about projects.
async fn check_environment(request: &ApiRequest<'_>) -> Result<(), MethodError> {
    let context = request.context;
    let config = &context.config;
    if config.value("projects.fields").is_some()
        || config
            .value("projects.custom-field-definitions")
            .is_some_and(php::is_truthy)
    {
        return Err(unsupported("project custom fields"));
    }
    let database = &context.database;
    let sql = format!(
        "SELECT id FROM {} LIMIT 1",
        database.table("spaces", "spaces_namespace")
    );
    if phab_db::sqlx::query(&sql)
        .fetch_optional(database.pool())
        .await?
        .is_some()
    {
        return Err(unsupported("projects in spaces"));
    }
    let rules = engine::load_rules(database, HERALD_CONTENT_TYPE)
        .await
        .map_err(|error| match error {
            HeraldError::Unsupported(what) => MethodError::Unsupported(what),
            other => MethodError::internal(other),
        })?;
    if !rules.is_empty() {
        return Err(unsupported("Herald rules about projects"));
    }
    Ok(())
}

/// `requireApplicationCapability()` or `requireCreateCapability()`:
/// `project.create`, which PHP checks before anything else.
pub(crate) async fn check_create_capability(request: &ApiRequest<'_>) -> Result<(), MethodError> {
    let context = request.context;
    let allowed = policy::has_application_capability(
        &context.database,
        &context.config,
        &request.viewer,
        (APPLICATION_CLASS, Capability("project.create")),
        request.now,
    )
    .await
    .map_err(MethodError::internal)?;
    if allowed {
        Ok(())
    } else {
        Err(unsupported("projects the actor may not create"))
    }
}

/// The slugs of a project, in ID order (`needSlugs()`).
async fn load_slugs(database: &Database, project: &str) -> Result<Vec<String>, MethodError> {
    let sql = format!(
        "SELECT slug FROM {} WHERE projectPHID = ? ORDER BY id",
        database.table("project", "project_slug")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(project)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| rows::text(row, "slug"))
        .collect::<Result<_, _>>()?)
}

/// The project owning `slug`, if any.
async fn slug_owner(database: &Database, slug: &str) -> Result<Option<String>, MethodError> {
    let sql = format!(
        "SELECT projectPHID FROM {} WHERE slug = ?",
        database.table("project", "project_slug")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(slug)
        .fetch_optional(database.pool())
        .await?
        .map(|row| rows::text(&row, "projectPHID"))
        .transpose()?)
}

/// The columns of an edited project [`Project`] doesn't have.
async fn load_extra(database: &Database, project: &Project) -> Result<Extra, MethodError> {
    let sql = format!(
        "SELECT isMembershipLocked, hasSubprojects FROM {} WHERE id = ?",
        database.table("project", "project")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(project.id)
        .fetch_one(database.pool())
        .await?;
    Ok(Extra {
        is_membership_locked: rows::flag(&row, "isMembershipLocked")?,
        has_subprojects: rows::flag(&row, "hasSubprojects")?,
    })
}

/// The validation errors of the name, which `validateTransactions()`
/// raises in this order.
async fn name_errors(
    database: &Database,
    (is_new, object): (bool, Option<&str>),
    name: Option<&str>,
) -> Result<Vec<String>, MethodError> {
    let Some(name) = name else {
        // `isEmptyTextTransaction()` of the current name.
        return Ok(if is_new {
            vec!["Projects must have a name.".to_owned()]
        } else {
            Vec::new()
        });
    };
    let mut errors = Vec::new();
    if name.is_empty() {
        if !is_new {
            // `setContinueOnMissingFields()` drops the error.
            return Err(unsupported("empty names on edits"));
        }
        errors.push("Projects must have a name.".to_owned());
    }
    let normal = search::valid_slug(name);
    if normal.is_none() {
        errors.push("Project names must contain at least one letter or number.".to_owned());
    }
    let slug = normal.unwrap_or_else(|| "_".to_owned());
    if let Some(owner) = slug_owner(database, &slug).await?
        && Some(owner.as_str()) != object
    {
        errors.push(format!(
            "Project name generates the same hashtag (\"#{slug}\") as another existing \
             project. Choose a unique name."
        ));
    }
    Ok(errors)
}

/// The hashtags of a `project:slugs` transaction must be valid and free:
/// PHP's messages for them depend on the translation.
async fn check_slugs(
    database: &Database,
    object: Option<&str>,
    slugs: &[String],
) -> Result<(), MethodError> {
    if !slugs.iter().all(|slug| is_plain(slug)) {
        return Err(unsupported("hashtags which are not printable ASCII"));
    }
    if slugs.iter().any(|slug| search::valid_slug(slug).is_none()) {
        return Err(unsupported("invalid hashtags"));
    }
    for slug in normalize_slugs(slugs) {
        if let Some(owner) = slug_owner(database, &slug).await?
            && Some(owner.as_str()) != object
        {
            return Err(unsupported("hashtags of other projects"));
        }
    }
    Ok(())
}

/// Whether every one of `phids` is a user who exists.
async fn users_exist(database: &Database, phids: &[String]) -> Result<bool, MethodError> {
    let mut unique: Vec<&str> = phids.iter().map(String::as_str).collect();
    unique.sort_unstable();
    unique.dedup();
    let plain = |phid: &&str| {
        phid.strip_prefix("PHID-USER-").is_some_and(|random| {
            !random.is_empty()
                && random
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
    };
    if !unique.iter().all(plain) {
        return Ok(false);
    }
    if unique.is_empty() {
        return Ok(true);
    }
    let sql = format!(
        "SELECT phid FROM {} WHERE phid IN ({})",
        database.table("user", "user"),
        phab_db::placeholders(unique.len())
    );
    let found = phab_db::bind_all(phab_db::sqlx::query(&sql), unique.iter().copied())
        .fetch_all(database.pool())
        .await?
        .len();
    Ok(found == unique.len())
}

/// `getLegacyRequiredEdgeCapabilities()` of a member edit: joining needs
/// `CAN_JOIN`, leaving nothing (`CAN_EDIT` for locked projects), anything
/// else `CAN_EDIT`.
fn member_capability(
    actor: &str,
    (removed, added): (&[String], &[String]),
    locked: bool,
) -> Option<Capability> {
    let only_actor = |list: &[String]| list.len() == 1 && list[0] == actor;
    if only_actor(added) && removed.is_empty() {
        Some(Capability::JOIN)
    } else if only_actor(removed) && added.is_empty() {
        locked.then_some(Capability::EDIT)
    } else {
        Some(Capability::EDIT)
    }
}

/// `applyCapabilityChecks()` on an existing project. Editors can always
/// join.
async fn check_capabilities(
    request: &ApiRequest<'_>,
    project: &Project,
    capabilities: &[Capability],
) -> Result<(), MethodError> {
    let context = request.context;
    let has = |capability| {
        policy::has_capability(
            &context.database,
            &context.config,
            &request.viewer,
            project,
            capability,
            request.now,
        )
    };
    let can_edit = if capabilities.is_empty() {
        true
    } else {
        has(Capability::EDIT).await.map_err(MethodError::internal)?
    };
    for capability in capabilities {
        let allowed = match *capability {
            Capability::JOIN => {
                can_edit || has(Capability::JOIN).await.map_err(MethodError::internal)?
            }
            _ => can_edit,
        };
        if !allowed {
            return Err(unsupported("edits the actor may not make"));
        }
    }
    Ok(())
}

/// A new transaction PHID of projects.
fn transaction_phid() -> String {
    Phid::generate(PhidType::TRANSACTION, Some(PhidType::PROJECT)).into_string()
}

/// The values of a transaction (`adjustTransactionValues()`) and whether
/// it has an effect, against the project before the edit.
fn plan_transaction(
    change: Change,
    state: &State,
    (slugs, lists): (&[String], &Lists),
    is_new: bool,
) -> (Planned, bool) {
    let string_or_null = |value: Option<&String>| value.map_or(Value::Null, |text| json!(text));
    let (kind, old, new, edge) = match &change {
        Change::Create => ("core:create", Value::Null, Value::Null, None),
        Change::Name(name) => {
            let old = if is_new { None } else { Some(&state.name) };
            ("project:name", string_or_null(old), json!(name), None)
        }
        Change::Icon(icon) => ("project:icon", json!(state.icon), json!(icon), None),
        Change::Color(color) => ("project:color", json!(state.color), json!(color), None),
        Change::Slugs(given) => {
            // `generateOldValue()`: the hashtags but the primary one.
            let mut old: Vec<String> = Vec::new();
            for slug in slugs {
                if Some(slug) != state.primary_slug.as_ref() && !old.contains(slug) {
                    old.push(slug.clone());
                }
            }
            (
                "project:slugs",
                json!(old),
                json!(normalize_slugs(given)),
                None,
            )
        }
        Change::Members(operation, phids) | Change::Watchers(operation, phids) => {
            let old = if matches!(change, Change::Members(..)) {
                &lists.members
            } else {
                &lists.watchers
            };
            let new = new_list(old, *operation, phids);
            let removed = minus(old, &new);
            let added = minus(&new, old);
            (
                "core:edge",
                json!(removed),
                json!(added),
                Some((removed, added)),
            )
        }
    };
    let effect = match (&change, &edge) {
        (Change::Create, _) => true,
        (_, Some((removed, added))) => !removed.is_empty() || !added.is_empty(),
        _ => old != new,
    };
    let planned = Planned {
        phid: transaction_phid(),
        kind,
        old,
        new,
        edge,
        change,
    };
    (planned, effect)
}

/// The `state` of the publish task (`getWorkerState()`).
fn worker_state(
    plan_state: &State,
    (actor, source): (&str, Source),
    is_new: bool,
    header: &str,
) -> Value {
    let stamp =
        |kind: &str, key: &str, value: Value| json!({"type": kind, "key": key, "value": value});
    let stamps = vec![
        stamp("string", "application", json!("Projects")),
        stamp("string", "phid", json!(plan_state.phid)),
        stamp("string", "object-type", json!("PROJ")),
        stamp("phid", "actor", json!(actor)),
        stamp("string", "via", json!(source.via())),
        stamp("bool", "silent", Value::Null),
        stamp("bool", "encrypted", json!([])),
        stamp("bool", "new", json!(is_new)),
        // Projects aren't subscribable: mentions are never computed.
        stamp("phid", "mention", Value::Null),
        stamp("string", "herald", herald_monograms(header)),
        stamp("phid", "removed", json!([])),
        // Without spaces, the stamp has no value.
        stamp("phid", "space", Value::Null),
    ];
    json!({
        "parentMessageID": null,
        "isNewObject": is_new,
        "heraldEmailPHIDs": [],
        "heraldForcedEmailPHIDs": [],
        "heraldHeader": header,
        "mailToPHIDs": [actor],
        "mailCCPHIDs": [],
        "feedNotifyPHIDs": [actor],
        "feedRelatedPHIDs": [plan_state.phid, actor],
        "feedShouldPublish": true,
        "mailShouldSend": true,
        "mustEncrypt": [],
        "mailStamps": stamps,
        "mailUnexpandablePHIDs": [],
        "mailMutedPHIDs": [],
        "webhookMap": [],
        "silent": null,
        "sendHistory": false,
        "excludeMailRecipientPHIDs": [],
        "custom": [],
        "custom.encoding": [],
    })
}

/// `loadXHeraldRulesHeader()`.
async fn saved_header(database: &Database, phid: &str) -> Result<Option<String>, MethodError> {
    let sql = format!(
        "SELECT header FROM {} WHERE phid = ?",
        database.table("herald", "herald_savedheader")
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(phid)
        .fetch_optional(database.pool())
        .await?
        .map(|row| rows::text(&row, "header"))
        .transpose()?)
}

/// The project before the edit, for a new one
/// (`PhabricatorProject::initializeNewProject()`).
fn new_state(request: &ApiRequest<'_>) -> Result<State, MethodError> {
    let config = &request.context.config;
    let (icon, color) = IconSet::load(config).default_keys();
    let (Some(icon), Some(color)) = (icon, color) else {
        return Err(unsupported("icon sets without defaults"));
    };
    let applications = phab_policy::ApplicationPolicies::from_config(config);
    let policy = |capability: &'static str| {
        applications
            .policy(APPLICATION_CLASS, Capability(capability), false)
            .to_owned()
    };
    Ok(State {
        id: None,
        phid: Phid::generate(PhidType::PROJECT, None).into_string(),
        name: String::new(),
        icon,
        color,
        primary_slug: None,
        slugs: Vec::new(),
        view_policy: policy("project.default.view"),
        edit_policy: policy("project.default.edit"),
        join_policy: policy("project.default.join"),
    })
}

/// `validateTransaction()` in the order of the editor's types, and what
/// the rows need: the errors PHP raises, or what is left to it.
async fn validate(
    database: &Database,
    (is_new, object): (bool, Option<&str>),
    state: &State,
    changes: &[Change],
) -> Result<(), MethodError> {
    let name = changes.iter().find_map(|change| match change {
        Change::Name(name) => Some(name.as_str()),
        _ => None,
    });
    if let Some(name) = name
        && (!is_plain(name) || name.len() > NAME_LENGTH)
    {
        return Err(unsupported("names which are not short printable ASCII"));
    }
    if !(is_new || is_plain(&state.name) && state.slugs.iter().all(|slug| is_plain(slug))) {
        return Err(unsupported(
            "projects whose names or hashtags aren't printable ASCII",
        ));
    }
    let errors = name_errors(database, (is_new, object), name).await?;
    if !errors.is_empty() {
        let mut message = "Validation errors:".to_owned();
        for error in errors {
            message.push_str("\n  - ");
            message.push_str(&error);
        }
        return Err(pager::core_error(message));
    }
    for change in changes {
        match change {
            Change::Slugs(given) => check_slugs(database, object, given).await?,
            Change::Members(_, phids) | Change::Watchers(_, phids) => {
                if !users_exist(database, phids).await? {
                    return Err(unsupported("members which are not users"));
                }
            }
            Change::Icon(value) | Change::Color(value) if value.len() > 32 => {
                return Err(unsupported("icons and colors longer than their column"));
            }
            _ => {}
        }
    }
    // A new primary hashtag must be free; MySQL refuses it otherwise.
    if let Some(name) = name {
        let slug = normalize_slug(name);
        let sql = format!(
            "SELECT phid FROM {} WHERE primarySlug = ?",
            database.table("project", "project")
        );
        let owner = phab_db::sqlx::query(&sql)
            .bind(&slug)
            .fetch_optional(database.pool())
            .await?
            .map(|row| rows::text(&row, "phid"))
            .transpose()?;
        if owner.is_some_and(|owner| Some(owner.as_str()) != object) {
            return Err(unsupported("primary hashtags of other projects"));
        }
    }
    Ok(())
}

/// An existing project before the edit, with its hashtags, its members
/// and the columns [`Project`] doesn't have. Subprojects, milestones,
/// parents and archived projects are left to PHP.
async fn existing_state(
    database: &Database,
    project: &Project,
) -> Result<(State, Vec<String>, Lists, Extra), MethodError> {
    if project.parent_phid.is_some() || project.is_milestone() {
        return Err(unsupported("subprojects and milestones"));
    }
    if project.is_archived() {
        return Err(unsupported("archived projects"));
    }
    let extra = load_extra(database, project).await?;
    if extra.has_subprojects {
        return Err(unsupported("projects with subprojects"));
    }
    let phid = project.phid.as_str();
    let slugs = load_slugs(database, phid).await?;
    let mut lists = Lists::default();
    for (edge_type, list) in [
        (edges::PROJECT_HAS_MEMBER, &mut lists.members),
        (OBJECT_HAS_WATCHER, &mut lists.watchers),
    ] {
        *list = edges::load_destination_phids(
            database,
            std::slice::from_ref(&project.phid),
            &[edge_type],
            None,
        )
        .await?
        .into_iter()
        .map(Phid::into_string)
        .collect();
    }
    let state = State {
        id: Some(project.id),
        phid: phid.to_owned(),
        name: project.name.clone(),
        icon: project.icon.clone(),
        color: project.color.clone(),
        primary_slug: project.primary_slug.clone(),
        slugs: slugs.clone(),
        view_policy: project.view_policy.clone(),
        edit_policy: project.edit_policy.clone(),
        join_policy: project.join_policy.clone(),
    };
    Ok((state, slugs, lists, extra))
}

/// Plans the edit of `target` by `changes`.
async fn plan(
    request: &ApiRequest<'_>,
    target: Target,
    changes: Vec<Change>,
    source: Source,
) -> Result<Plan, MethodError> {
    let context = request.context;
    let database = &context.database;
    let actor = request
        .viewer
        .phid
        .as_ref()
        .map(|phid| phid.as_str().to_owned())
        .ok_or_else(|| unsupported("edits by the omnipotent user"))?;
    check_environment(request).await?;

    // At most one transaction of each type: PHP merges or repeats them.
    for (index, change) in changes.iter().enumerate() {
        let same = |other: &Change| std::mem::discriminant(other) == std::mem::discriminant(change);
        if changes[..index].iter().any(same) {
            return Err(unsupported("several transactions of one type"));
        }
    }
    let is_new = target.project.is_none();
    let (mut state, slugs, lists, extra) = match &target.project {
        None => (new_state(request)?, Vec::new(), Lists::default(), None),
        Some(project) => {
            let (state, slugs, lists, extra) = existing_state(database, project).await?;
            (state, slugs, lists, Some(extra))
        }
    };
    let object = target.project.as_ref().map(|project| project.phid.as_str());

    validate(database, (is_new, object), &state, &changes).await?;

    let mut transactions = Vec::new();
    let mut capabilities = Vec::new();
    for change in changes {
        let (planned, effect) = plan_transaction(change, &state, (&slugs, &lists), is_new);
        if let Some(extra) = &extra {
            let capability = match (&planned.change, &planned.edge) {
                // See PHI1024: watching needs no capability.
                (Change::Watchers(..), _) => None,
                (_, Some((removed, added))) => {
                    member_capability(&actor, (removed, added), extra.is_membership_locked)
                }
                (_, None) => Some(Capability::EDIT),
            };
            capabilities.extend(capability);
        }
        if effect {
            transactions.push(planned);
        }
    }
    if let Some(project) = &target.project {
        check_capabilities(request, project, &capabilities).await?;
        if transactions.is_empty() {
            return Err(unsupported("edits without effect"));
        }
    }

    // applyInternalEffects().
    for planned in &transactions {
        match &planned.change {
            Change::Name(name) => {
                state.name.clone_from(name);
                state.primary_slug = Some(normalize_slug(name));
            }
            Change::Icon(icon) => state.icon.clone_from(icon),
            Change::Color(color) => state.color.clone_from(color),
            _ => {}
        }
    }

    let applied: Vec<String> = transactions
        .iter()
        .map(|planned| planned.phid.clone())
        .collect();
    let (transcript, herald_header, worker_data) =
        publication(database, &state, (&actor, source, is_new), applied).await?;
    Ok(Plan {
        actor,
        source,
        state,
        transactions,
        group_id: random::random_characters(32),
        transcript,
        herald_header,
        worker_data,
    })
}

/// Herald, with no rule to evaluate, and the publish task's data.
async fn publication(
    database: &Database,
    state: &State,
    (actor, source, is_new): (&str, Source, bool),
    applied: Vec<String>,
) -> Result<(engine::TranscriptRow, String, String), MethodError> {
    let subject = Subject {
        phid: state.phid.clone(),
        content_type: HERALD_CONTENT_TYPE.to_owned(),
        name: format!("Project {}", state.name),
        applied_transaction_phids: Some(applied.clone()),
    };
    let evaluation = engine::evaluate(&[], &subject, &[]);
    let transcript = evaluation.transcript(&subject, &[]);
    let saved = if is_new {
        None
    } else {
        saved_header(database, &state.phid).await?
    };
    let herald_header = engine::combine_headers(saved.as_deref(), &engine::rules_header(&[]));
    let worker_data = php::json_encode(&json!({
        "objectPHID": state.phid,
        "actorPHID": actor,
        "xactionPHIDs": applied,
        "state": worker_state(state, (actor, source), is_new, &herald_header),
    }));
    Ok((transcript, herald_header, worker_data))
}

/// `PhabricatorProjectTransactionEditor::addSlug()`.
async fn add_slug(
    connection: &mut MySqlConnection,
    database: &Database,
    (project, slug, force): (&str, &str, bool),
    now: i64,
) -> Result<(), MethodError> {
    let slug = normalize_slug(slug);
    let table = database.table("project", "project_slug");
    let current = if force {
        phab_db::sqlx::query(&format!(
            "SELECT id FROM {table} WHERE slug = ? AND projectPHID = ?"
        ))
        .bind(&slug)
        .bind(project)
        .fetch_optional(&mut *connection)
        .await?
    } else {
        phab_db::sqlx::query(&format!("SELECT id FROM {table} WHERE slug = ?"))
            .bind(&slug)
            .fetch_optional(&mut *connection)
            .await?
    };
    if current.is_none() {
        phab_db::sqlx::query(&format!(
            "INSERT INTO {table} (projectPHID, slug, dateCreated, dateModified) VALUES (?, ?, ?, ?)"
        ))
        .bind(project)
        .bind(&slug)
        .bind(now)
        .bind(now)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

/// `removeSlugs()`: the literal and the normalized hashtags.
async fn remove_slugs(
    connection: &mut MySqlConnection,
    database: &Database,
    project: &str,
    slugs: &[String],
) -> Result<(), MethodError> {
    if slugs.is_empty() {
        return Ok(());
    }
    let mut all = slugs.to_vec();
    all.extend(normalize_slugs(slugs));
    phab_db::bind_all(
        phab_db::sqlx::query(&format!(
            "DELETE FROM {} WHERE projectPHID = ? AND slug IN ({})",
            database.table("project", "project_slug"),
            phab_db::placeholders(all.len())
        ))
        .bind(project),
        all.iter().map(String::as_str),
    )
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// `PhabricatorProjectsMembershipIndexEngineExtension::materializeProject()`
/// of a root project without subprojects: its members are its
/// materialized members.
async fn rematerialize(
    connection: &mut MySqlConnection,
    database: &Database,
    (id, phid): (i64, &str),
) -> Result<(), MethodError> {
    let projects = database.table("project", "project");
    let edge = database.table("project", "edge");
    let has_milestones = phab_db::sqlx::query(&format!(
        "SELECT id FROM {projects} WHERE parentProjectPHID = ? AND milestoneNumber IS NOT NULL \
         LIMIT 1"
    ))
    .bind(phid)
    .fetch_optional(&mut *connection)
    .await?
    .is_some();
    let have: Vec<String> = phab_db::sqlx::query(&format!(
        "SELECT dst FROM {edge} WHERE src = ? AND type = ?"
    ))
    .bind(phid)
    .bind(MATERIALIZED_MEMBER)
    .fetch_all(&mut *connection)
    .await?
    .iter()
    .map(|row| rows::text(row, "dst"))
    .collect::<Result<_, _>>()?;
    let want: Vec<(String, i64, i64)> = phab_db::sqlx::query(&format!(
        "SELECT dst, dateCreated, seq FROM {edge} WHERE src = ? AND type = ?"
    ))
    .bind(phid)
    .bind(edges::PROJECT_HAS_MEMBER)
    .fetch_all(&mut *connection)
    .await?
    .iter()
    .map(|row| {
        Ok((
            rows::text(row, "dst")?,
            rows::int(row, "dateCreated")?,
            rows::int(row, "seq")?,
        ))
    })
    .collect::<Result<_, phab_db::DbError>>()?;
    let removed: Vec<&String> = have
        .iter()
        .filter(|dst| !want.iter().any(|(wanted, _, _)| wanted == *dst))
        .collect();
    if !removed.is_empty() {
        phab_db::bind_all(
            phab_db::sqlx::query(&format!(
                "DELETE FROM {edge} WHERE src = ? AND type = ? AND dst IN ({})",
                phab_db::placeholders(removed.len())
            ))
            .bind(phid)
            .bind(MATERIALIZED_MEMBER),
            removed.iter().map(|dst| dst.as_str()),
        )
        .execute(&mut *connection)
        .await?;
    }
    for (dst, created, seq) in want.iter().filter(|(dst, _, _)| !have.contains(dst)) {
        phab_db::sqlx::query(&format!(
            "INSERT IGNORE INTO {edge} (src, type, dst, dateCreated, seq) VALUES (?, ?, ?, ?, ?)"
        ))
        .bind(phid)
        .bind(MATERIALIZED_MEMBER)
        .bind(dst)
        .bind(created)
        .bind(seq)
        .execute(&mut *connection)
        .await?;
    }
    for (column, value) in [("hasSubprojects", false), ("hasMilestones", has_milestones)] {
        phab_db::sqlx::query(&format!("UPDATE {projects} SET {column} = ? WHERE id = ?"))
            .bind(value)
            .bind(id)
            .execute(&mut *connection)
            .await?;
    }
    Ok(())
}

/// The project row: inserted (`initializeNewProject()` then `save()`) or
/// updated. Returns its ID.
async fn write_project(
    connection: &mut MySqlConnection,
    database: &Database,
    plan: &Plan,
    now: i64,
) -> Result<i64, MethodError> {
    let state = &plan.state;
    let table = database.table("project", "project");
    if let Some(id) = state.id {
        phab_db::sqlx::query(&format!(
            "UPDATE {table} SET name = ?, primarySlug = ?, icon = ?, color = ?, dateModified = ? \
             WHERE id = ?"
        ))
        .bind(&state.name)
        .bind(state.primary_slug.as_deref())
        .bind(&state.icon)
        .bind(&state.color)
        .bind(now)
        .bind(id)
        .execute(&mut *connection)
        .await?;
        return Ok(id);
    }
    let path_key: String = hash::digest_for_index(state.phid.as_bytes())
        .chars()
        .take(4)
        .collect();
    let id = phab_db::sqlx::query(&format!(
        "INSERT INTO {table} (name, phid, authorPHID, dateCreated, dateModified, status, \
         viewPolicy, editPolicy, joinPolicy, isMembershipLocked, profileImagePHID, icon, color, \
         mailKey, primarySlug, parentProjectPHID, hasWorkboard, hasMilestones, hasSubprojects, \
         milestoneNumber, projectPath, projectDepth, projectPathKey, properties, spacePHID, \
         subtype) VALUES (?, ?, ?, ?, ?, '0', ?, ?, ?, 0, NULL, ?, ?, ?, ?, NULL, 0, 0, 0, NULL, \
         ?, 0, ?, '[]', NULL, 'default')"
    ))
    .bind(&state.name)
    .bind(&state.phid)
    .bind(&plan.actor)
    .bind(now)
    .bind(now)
    .bind(&state.view_policy)
    .bind(&state.edit_policy)
    .bind(&state.join_policy)
    .bind(&state.icon)
    .bind(&state.color)
    .bind(random::random_characters(20))
    .bind(state.primary_slug.as_deref())
    .bind(&path_key)
    .bind(&path_key)
    .execute(&mut *connection)
    .await?
    .last_insert_id();
    i64::try_from(id).map_err(MethodError::internal)
}

/// `updateDatasourceTokens()`: the hashtags the project had when it was
/// saved, and its name.
async fn write_tokens(
    connection: &mut MySqlConnection,
    database: &Database,
    (id, state): (i64, &State),
) -> Result<(), MethodError> {
    let table = database.table("project", "project_datasourcetoken");
    let mut strings = state.slugs.clone();
    strings.push(state.name.clone());
    let tokens = tokenize(&strings.join(" "));
    phab_db::sqlx::query(&format!("DELETE FROM {table} WHERE projectID = ?"))
        .bind(id)
        .execute(&mut *connection)
        .await?;
    if !tokens.is_empty() {
        let sql = format!(
            "INSERT INTO {table} (projectID, token) VALUES {}",
            vec!["(?, ?)"; tokens.len()].join(", ")
        );
        let mut query = phab_db::sqlx::query(&sql);
        for token in &tokens {
            query = query.bind(id).bind(token.as_str());
        }
        query.execute(&mut *connection).await?;
    }
    Ok(())
}

/// The transcript and the saved header (`HeraldEngine::applyEffects()`
/// with no rules).
async fn write_herald(
    request: &ApiRequest<'_>,
    connection: &mut MySqlConnection,
    plan: &Plan,
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let transcript = &plan.transcript;
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, time, host, duration, objectPHID, dryRun, objectTranscript, \
         ruleTranscripts, conditionTranscripts, applyTranscripts, garbageCollected) VALUES \
         (?, ?, ?, ?, ?, 0, ?, ?, ?, ?, 0)",
        database.table("herald", "herald_transcript")
    ))
    .bind(Phid::generate(PhidType::HERALD_TRANSCRIPT, None).as_str())
    .bind(request.now)
    .bind(&request.context.hostname)
    .bind(transcript.duration)
    .bind(&transcript.object_phid)
    .bind(&transcript.object_transcript)
    .bind(&transcript.rule_transcripts)
    .bind(&transcript.condition_transcripts)
    .bind(&transcript.apply_transcripts)
    .execute(&mut *connection)
    .await?;
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (phid, header) VALUES (?, ?) ON DUPLICATE KEY UPDATE header = \
         VALUES(header)",
        database.table("herald", "herald_savedheader")
    ))
    .bind(&plan.state.phid)
    .bind(&plan.herald_header)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// Writes the planned edit in one transaction, in PHP's order. Returns the
/// project ID.
async fn write(request: &ApiRequest<'_>, plan: &Plan) -> Result<i64, MethodError> {
    let database = &request.context.database;
    let now = request.now;
    let mut transaction = database.pool().begin().await?;
    let connection = &mut *transaction;
    let state = &plan.state;
    let project = state.phid.as_str();

    let id = write_project(connection, database, plan, now).await?;
    write_tokens(connection, database, (id, state)).await?;

    let table = database.table("project", "project_transaction");
    // `applyTransactions()` marks every transaction of an edit with a
    // `core:create` one, before it sets the group.
    let create = plan
        .transactions
        .iter()
        .any(|planned| matches!(planned.change, Change::Create));
    for planned in &plan.transactions {
        let mut metadata = serde_json::Map::new();
        if let Some((edge_type, _)) = planned.change.edge_types() {
            metadata.insert("edge:type".to_owned(), json!(edge_type));
        }
        if create {
            metadata.insert("core.create".to_owned(), json!(true));
        }
        metadata.insert("core.groupID".to_owned(), json!(plan.group_id));
        let metadata = Value::Object(metadata);
        let (old, new, metadata) = (
            php::json_encode(&planned.old),
            php::json_encode(&planned.new),
            php::json_encode(&metadata),
        );
        let row = TransactionRow {
            phid: &planned.phid,
            author: &plan.actor,
            object: project,
            kind: planned.kind,
            old: &old,
            new: &new,
            metadata: &metadata,
            content_source: plan.source.content_source(),
        };
        db_editor::insert_transaction(connection, &table, row, now).await?;
    }

    // applyExternalEffects(), in the order of the transactions.
    let mut materialize = false;
    for planned in &plan.transactions {
        match (&planned.change, &planned.edge) {
            (Change::Name(name), _) => {
                if let Some(old) = planned.old.as_str() {
                    add_slug(connection, database, (project, old, false), now).await?;
                }
                add_slug(connection, database, (project, name, false), now).await?;
            }
            (Change::Slugs(_), _) => {
                let list = |value: &Value| -> Vec<String> {
                    value
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                };
                let (old, new) = (list(&planned.old), list(&planned.new));
                for slug in minus(&new, &old) {
                    add_slug(connection, database, (project, &slug, true), now).await?;
                }
                remove_slugs(connection, database, project, &minus(&old, &new)).await?;
            }
            (change, Some((removed, added))) => {
                let Some((edge_type, inverse)) = change.edge_types() else {
                    continue;
                };
                let types = (project, edge_type, inverse);
                db_editor::save_edges(connection, database, types, (removed, added), now).await?;
                materialize |= edge_type == edges::PROJECT_HAS_MEMBER;
            }
            _ => {}
        }
    }

    // applyFinalEffects().
    if materialize {
        rematerialize(connection, database, (id, project)).await?;
    }
    phab_db::sqlx::query(&format!(
        "UPDATE {} SET spacePHID = NULL WHERE parentProjectPHID = ? AND milestoneNumber IS NOT \
         NULL",
        database.table("project", "project")
    ))
    .bind(project)
    .execute(&mut *connection)
    .await?;

    write_herald(request, connection, plan).await?;
    let task = worker::Task {
        class: "PhabricatorApplicationTransactionPublishWorker",
        data: &plan.worker_data,
        priority: worker::PRIORITY_ALERTS,
        object_phid: Some(project),
    };
    worker::schedule_task(connection, database, task, now).await?;
    transaction.commit().await?;
    Ok(id)
}

/// Applies `changes` to `target` as `PhabricatorProjectTransactionEditor`
/// does.
pub(crate) async fn apply(
    request: &ApiRequest<'_>,
    target: Target,
    changes: Vec<Change>,
    source: Source,
) -> Result<Outcome, MethodError> {
    let plan = plan(request, target, changes, source).await?;
    let id = write(request, &plan).await?;
    Ok(Outcome {
        id,
        phid: plan.state.phid.clone(),
        name: plan.state.name.clone(),
        icon: plan.state.icon.clone(),
        color: plan.state.color.clone(),
        transactions: plan
            .transactions
            .iter()
            .map(|planned| planned.phid.clone())
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn tokenizes_like_php() {
        assert_eq!(
            tokenize("build_reviewers Build (Reviewers) [x]-y"),
            list(&["build_reviewers", "build", "reviewers", "x", "y"])
        );
        assert_eq!(tokenize("  "), Vec::<String>::new());
    }

    #[test]
    fn normalizes_slugs_like_php() {
        assert_eq!(
            normalize_slugs(&list(&["Foo Bar", "foo_bar", "??", "."])),
            list(&["foo_bar", "_", "dot"])
        );
    }

    #[test]
    fn lists_monograms_like_php() {
        assert_eq!(herald_monograms("none"), json!([]));
        assert_eq!(herald_monograms("<1>, <2>, none"), json!(["H1", "H2"]));
        assert_eq!(herald_monograms("none, <3>"), json!({"1": "H3"}));
    }

    #[test]
    fn edits_member_lists_like_php() {
        let old = list(&["a", "b"]);
        assert_eq!(
            new_list(&old, Operation::Add, &list(&["c", "a"])),
            list(&["a", "b", "c"])
        );
        assert_eq!(
            new_list(&old, Operation::Remove, &list(&["a"])),
            list(&["b"])
        );
        assert_eq!(
            new_list(&old, Operation::Set, &list(&["c", "b"])),
            list(&["b", "c"])
        );
    }

    #[test]
    fn requires_capabilities_like_php() {
        let me = "PHID-USER-me";
        let none: &[String] = &[];
        let only_me = list(&[me]);
        assert_eq!(
            member_capability(me, (none, &only_me), false),
            Some(Capability::JOIN)
        );
        assert_eq!(member_capability(me, (&only_me, none), false), None);
        assert_eq!(
            member_capability(me, (&only_me, none), true),
            Some(Capability::EDIT)
        );
        assert_eq!(
            member_capability(me, (none, &list(&["PHID-USER-x"])), false),
            Some(Capability::EDIT)
        );
        assert_eq!(
            member_capability(me, (none, none), false),
            Some(Capability::EDIT)
        );
    }
}
