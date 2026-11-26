//! `differential.revision.search`: `DifferentialRevisionSearchEngine`
//! through `PhabricatorApplicationSearchEngine::buildConduitResponse()`.
//!
//! Ported: the `ids`, `phids`, `authorPHIDs`, `responsiblePHIDs`,
//! `reviewerPHIDs`, `repositoryPHIDs`, `statuses`, `subscribers`,
//! `projects` and date constraints (with plain values, not usernames nor
//! functions like `viewer()`; projects without subprojects or
//! milestones), the builtin orders but `relevance`, paging, every field
//! (Mozilla's `aiChange` too) and every attachment. `query`,
//! `affectedPaths`, order vectors and saved queries are left to PHP
//! (`MethodError::Unsupported`).

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use phab_conduit::pager::{self, Direction, Paging, PolicyAwareQuery};
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::Phid;
use phab_db::{Database, rows};
use serde_json::{Map, Value, json};

use crate::ai_change::{self, AiChange};
use crate::merge_conflict;
use crate::storage::{self, Revision};

/// `PhabricatorDifferentialApplication`.
pub const APPLICATION: Application = Application {
    class: "PhabricatorDifferentialApplication",
    name: "Differential",
};

const QUERY_CLASS: &str = "DifferentialRevisionQuery";

/// The constraint keys of the engine's search fields, as PHP lists them.
const VALID_CONSTRAINTS: &[&str] = &[
    "responsiblePHIDs",
    "authorPHIDs",
    "reviewerPHIDs",
    "repositoryPHIDs",
    "statuses",
    "createdStart",
    "createdEnd",
    "modifiedStart",
    "modifiedEnd",
    "affectedPaths",
    "query",
    "ids",
    "phids",
    "subscribers",
    "projects",
];

/// The keys of `getConduitSearchAttachments()`.
const ATTACHMENTS: &[&str] = &["reviewers", "subscribers", "projects", "reviewers-extra"];

/// The custom field keys of `DifferentialBugzillaBugIDField` and
/// `DifferentialUpliftRequestCustomField` (moz-extensions).
pub(crate) const BUG_FIELD: &str = "differential:bugzilla-bug-id";
pub(crate) const UPLIFT_FIELD: &str = "differential:uplift-request";

/// The ported constraints.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Constraints {
    pub(crate) ids: Option<Vec<i64>>,
    pub(crate) phids: Option<Vec<String>>,
    pub(crate) authors: Option<Vec<String>>,
    pub(crate) repositories: Option<Vec<String>>,
    pub(crate) statuses: Option<Vec<String>>,
    pub(crate) created: (Option<i64>, Option<i64>),
    pub(crate) modified: (Option<i64>, Option<i64>),
    /// `withReviewers()`.
    pub(crate) reviewers: Option<Vec<String>>,
    /// `withResponsibleUsers()`, as given; [`Constraints::resolve()`]
    /// adds the projects of the users.
    pub(crate) responsibles: Option<Vec<String>>,
    /// The subscribers extension: subscribed by any of them.
    pub(crate) subscribers: Option<Vec<String>>,
    /// The projects extension: tagged with each of them.
    pub(crate) projects: Option<Vec<String>>,
}

impl Constraints {
    /// `constraints`: unknown keys are an error, as in PHP; known keys
    /// which are not ported go to PHP.
    fn parse(value: Option<&Value>) -> Result<Self, MethodError> {
        let entries: Vec<(String, &Value)> = match value {
            None => return Ok(Self::default()),
            Some(Value::Object(map)) => map
                .iter()
                .map(|(key, value)| (key.clone(), value))
                .collect(),
            Some(Value::Array(list)) => list
                .iter()
                .enumerate()
                .map(|(index, value)| (index.to_string(), value))
                .collect(),
            Some(_) => return Err(MethodError::Unsupported("constraints which are not a map")),
        };
        if let Some((key, _)) = entries
            .iter()
            .find(|(key, _)| !VALID_CONSTRAINTS.contains(&key.as_str()))
        {
            return Err(pager::core_error(format!(
                "Constraint \"{key}\" is not a valid constraint for this query."
            )));
        }
        let mut constraints = Self::default();
        for (key, value) in entries {
            match key.as_str() {
                "ids" => {
                    let ids = nonempty_list(&key, value)?
                        .iter()
                        .enumerate()
                        .map(|(index, id)| {
                            id.as_i64().filter(|_| !id.is_f64()).ok_or_else(|| {
                                pager::core_error(format!(
                                    "Error while reading \"ids[{index}]\": Expected integer, \
                                     got something else."
                                ))
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    constraints.ids = Some(ids);
                }
                "phids" => {
                    let phids = nonempty_list(&key, value)?
                        .iter()
                        .map(|phid| phid.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()
                        .ok_or(MethodError::Unsupported("phids which are not strings"))?;
                    constraints.phids = Some(phids);
                }
                // ConduitUserListParameterType allows an empty list.
                "authorPHIDs" => {
                    let authors = match value {
                        Value::Array(list) if list.is_empty() => continue,
                        _ => plain_phids(nonempty_list(&key, value)?, "PHID-USER-")?,
                    };
                    constraints.authors = Some(authors);
                }
                "repositoryPHIDs" => {
                    constraints.repositories =
                        Some(plain_phids(nonempty_list(&key, value)?, "PHID-REPO-")?);
                }
                "statuses" => {
                    let statuses = nonempty_list(&key, value)?
                        .iter()
                        .map(|status| {
                            status
                                .as_str()
                                .filter(|status| !status.contains('('))
                                .map(str::to_owned)
                        })
                        .collect::<Option<Vec<_>>>()
                        .ok_or(MethodError::Unsupported("status functions"))?;
                    constraints.statuses = Some(statuses);
                }
                "reviewerPHIDs" | "responsiblePHIDs" | "subscribers" | "projects" => {
                    constraints.parse_relation(&key, value)?;
                }
                "createdStart" => constraints.created.0 = Some(epoch(&key, value)?),
                "createdEnd" => constraints.created.1 = Some(epoch(&key, value)?),
                "modifiedStart" => constraints.modified.0 = Some(epoch(&key, value)?),
                "modifiedEnd" => constraints.modified.1 = Some(epoch(&key, value)?),
                _ => {
                    return Err(MethodError::Unsupported(
                        "a revision search constraint not ported",
                    ));
                }
            }
        }
        Ok(constraints)
    }
}

impl Constraints {
    /// The constraints on who reviews, is responsible for, subscribes to
    /// and tags the revision. Datasource fields refuse an empty list;
    /// `ConduitUserListParameterType` and `ConduitProjectListParameterType`
    /// allow it. Functions like `none()` or `viewer()`, usernames and
    /// hashtags go to PHP.
    fn parse_relation(&mut self, key: &str, value: &Value) -> Result<(), MethodError> {
        let allows_empty = key == "subscribers" || key == "projects";
        if allows_empty && matches!(value, Value::Array(list) if list.is_empty()) {
            return Ok(());
        }
        let list = nonempty_list(key, value)?;
        let prefix = if key == "projects" {
            "PHID-PROJ-"
        } else {
            "PHID-"
        };
        let phids = Some(plain_phids(list, prefix)?);
        match key {
            "reviewerPHIDs" => self.reviewers = phids,
            "responsiblePHIDs" => self.responsibles = phids,
            "subscribers" => self.subscribers = phids,
            _ => self.projects = phids,
        }
        Ok(())
    }
}

/// `column IN (...)` with its values.
fn in_list<'v>(column: &str, values: impl IntoIterator<Item = &'v String>) -> (String, Vec<Value>) {
    let values: Vec<Value> = values.into_iter().map(|value| json!(value)).collect();
    (
        format!("{column} IN ({})", phab_db::placeholders(values.len())),
        values,
    )
}

/// `DifferentialReviewerStatus::STATUS_RESIGNED`.
const RESIGNED: &str = "resigned";

impl Constraints {
    /// What PHP's datasources add to the values as given: the projects of
    /// responsible users (`expandResponsibleUsers()`). Projects with
    /// subprojects or milestones, which also match their descendants, are
    /// left to PHP.
    async fn resolve(&mut self, request: &ApiRequest<'_>) -> Result<(), MethodError> {
        let context = request.context;
        let database = &context.database;
        if let Some(projects) = &self.projects {
            let sql = format!(
                "SELECT phid FROM {} WHERE parentProjectPHID IN ({}) LIMIT 1",
                database.table("project", "project"),
                phab_db::placeholders(projects.len())
            );
            let children = phab_db::bind_all(phab_db::sqlx::query(&sql), projects.iter())
                .fetch_all(database.pool())
                .await?;
            if !children.is_empty() {
                return Err(MethodError::Unsupported("projects with subprojects"));
            }
        }
        let Some(responsibles) = &mut self.responsibles else {
            return Ok(());
        };
        let users: Vec<&String> = responsibles
            .iter()
            .filter(|phid| phid.starts_with("PHID-USER-"))
            .collect();
        if users.is_empty() {
            return Ok(());
        }
        // PhabricatorProjectQuery::withMemberPHIDs(): the materialized
        // members.
        let sql = format!(
            "SELECT DISTINCT src FROM {} WHERE type = ? AND dst IN ({})",
            database.table("project", "edge"),
            phab_db::placeholders(users.len())
        );
        let query = phab_db::sqlx::query(&sql).bind(phab_db::edges::PROJECT_MATERIALIZED_MEMBER);
        let rows = phab_db::bind_all(query, users.iter().map(|phid| phid.as_str()))
            .fetch_all(database.pool())
            .await?;
        let phids: Vec<Phid> = rows
            .iter()
            .map(|row| rows::text(row, "src").map(Phid::new))
            .collect::<Result<_, _>>()?;
        let projects = phab_app_project::storage::load_by_phid(database, &phids).await?;
        let (visible, _) = phab_app_project::policy::visible_projects(
            database,
            &context.config,
            &request.viewer,
            projects,
            request.now,
        )
        .await
        .map_err(MethodError::internal)?;
        responsibles.extend(
            visible
                .into_iter()
                .map(|project| project.phid.into_string()),
        );
        Ok(())
    }

    /// The condition that a reviewer of these, who did not resign, reviews
    /// the revision (the `reviewer` join).
    fn reviewed_by(database: &Database, reviewers: &[&String]) -> (String, Vec<Value>) {
        let (list, mut binds) = in_list("reviewerPHID", reviewers.iter().copied());
        binds.insert(0, json!(RESIGNED));
        (
            format!(
                "phid IN (SELECT revisionPHID FROM {} WHERE reviewerStatus != ? AND {list})",
                database.table("differential", "differential_reviewer")
            ),
            binds,
        )
    }

    /// The condition that the revision has an edge of `edge_type` to one
    /// of `destinations` (`buildEdgeLogicJoinClause()`).
    fn has_edge(
        database: &Database,
        edge_type: i64,
        destinations: &[&String],
    ) -> (String, Vec<Value>) {
        let (list, mut binds) = in_list("dst", destinations.iter().copied());
        binds.insert(0, json!(edge_type));
        (
            format!(
                "phid IN (SELECT src FROM {} WHERE type = ? AND {list})",
                database.table("differential", "edge")
            ),
            binds,
        )
    }

    /// The author and reviewer conditions. With responsible users, PHP
    /// unions two queries: one where they are authors, one where they (or
    /// their projects) review.
    fn people_conditions(&self, database: &Database) -> Vec<(String, Vec<Value>)> {
        let authors: Vec<&String> = self.authors.iter().flatten().collect();
        let reviewers: Vec<&String> = self.reviewers.iter().flatten().collect();
        let Some(responsibles) = &self.responsibles else {
            let mut parts = Vec::new();
            if self.authors.is_some() {
                parts.push(in_list("authorPHID", authors));
            }
            if self.reviewers.is_some() {
                parts.push(Self::reviewed_by(database, &reviewers));
            }
            return parts;
        };
        let all = |parts: Vec<(String, Vec<Value>)>| {
            let sql: Vec<String> = parts.iter().map(|(sql, _)| format!("({sql})")).collect();
            (
                sql.join(" AND "),
                parts
                    .into_iter()
                    .flat_map(|(_, binds)| binds)
                    .collect::<Vec<_>>(),
            )
        };
        let mut as_authors = vec![in_list(
            "authorPHID",
            authors.iter().copied().chain(responsibles),
        )];
        if !reviewers.is_empty() {
            as_authors.push(Self::reviewed_by(database, &reviewers));
        }
        let mut as_reviewers = Vec::new();
        if !authors.is_empty() {
            as_reviewers.push(in_list("authorPHID", authors.iter().copied()));
        }
        let all_reviewers: Vec<&String> = reviewers.iter().copied().chain(responsibles).collect();
        as_reviewers.push(Self::reviewed_by(database, &all_reviewers));
        let (first, mut binds) = all(as_authors);
        let (second, more) = all(as_reviewers);
        binds.extend(more);
        vec![(format!("({first}) OR ({second})"), binds)]
    }

    /// The `WHERE` conditions of the constraints (`buildWhereClauseParts()`),
    /// and the values they bind.
    pub(crate) fn add_conditions(
        &self,
        database: &Database,
        conditions: &mut Vec<String>,
        binds: &mut Vec<Value>,
    ) {
        let mut parts = self.people_conditions(database);
        if let Some(subscribers) = &self.subscribers {
            let subscribers: Vec<&String> = subscribers.iter().collect();
            parts.push(Self::has_edge(
                database,
                phab_db::edges::OBJECT_HAS_SUBSCRIBER,
                &subscribers,
            ));
        }
        // Each project is a constraint of its own.
        for project in self.projects.iter().flatten() {
            parts.push(Self::has_edge(
                database,
                phab_db::edges::OBJECT_HAS_PROJECT,
                &[project],
            ));
        }
        for (sql, values) in parts {
            conditions.push(format!("({sql})"));
            binds.extend(values);
        }
        let mut in_list = |column: &str, values: Vec<Value>| {
            conditions.push(format!(
                "{column} IN ({})",
                phab_db::placeholders(values.len())
            ));
            binds.extend(values);
        };
        if let Some(ids) = &self.ids {
            in_list("id", ids.iter().map(|id| json!(id)).collect());
        }
        if let Some(phids) = &self.phids {
            in_list("phid", phids.iter().map(|phid| json!(phid)).collect());
        }
        if let Some(repositories) = &self.repositories {
            in_list(
                "repositoryPHID",
                repositories.iter().map(|phid| json!(phid)).collect(),
            );
        }
        if let Some(statuses) = &self.statuses {
            in_list(
                "status",
                statuses.iter().map(|status| json!(status)).collect(),
            );
        }
        // withCreatedEpochBetween() and withUpdatedEpochBetween().
        for (column, (start, end)) in [
            ("dateCreated", self.created),
            ("dateModified", self.modified),
        ] {
            if let Some(start) = start {
                conditions.push(format!("{column} >= ?"));
                binds.push(json!(start));
            }
            if let Some(end) = end {
                conditions.push(format!("{column} <= ?"));
                binds.push(json!(end));
            }
        }
    }
}

/// The value of a list constraint (`ConduitListParameterType`), with
/// PHP's errors for what is not a nonempty list.
fn nonempty_list<'v>(key: &str, value: &'v Value) -> Result<&'v Vec<Value>, MethodError> {
    let error = |what: &str| {
        Err(pager::core_error(format!(
            "Error while reading \"{key}\": Expected {what}."
        )))
    };
    match value {
        Value::Array(list) if list.is_empty() => {
            error("a nonempty list, but value is an empty list")
        }
        Value::Array(list) => Ok(list),
        // PHP decodes {"0": ...} into a list.
        Value::Object(map)
            if map
                .keys()
                .all(|key| key.bytes().all(|b| b.is_ascii_digit())) =>
        {
            Err(MethodError::Unsupported("constraint lists sent as objects"))
        }
        Value::Object(_) => error("a list, but value is an object"),
        _ => error("a list, but value is not a list"),
    }
}

/// PHIDs of one type, as given; anything else (usernames, functions like
/// `viewer()`) is resolved by PHP.
fn plain_phids(list: &[Value], prefix: &str) -> Result<Vec<String>, MethodError> {
    list.iter()
        .map(|phid| {
            phid.as_str()
                .filter(|phid| phid.starts_with(prefix))
                .map(str::to_owned)
        })
        .collect::<Option<Vec<_>>>()
        .ok_or(MethodError::Unsupported(
            "constraint values which are not PHIDs",
        ))
}

/// `ConduitEpochParameterType`: a positive integer.
fn epoch(key: &str, value: &Value) -> Result<i64, MethodError> {
    match value {
        Value::Number(number) if !number.is_f64() => {
            let epoch = number
                .as_i64()
                .ok_or(MethodError::Unsupported("huge epochs"))?;
            if epoch <= 0 {
                return Err(pager::core_error(format!(
                    "Error while reading \"{key}\": Epoch timestamp must be larger than 0, \
                     got {epoch}."
                )));
            }
            Ok(epoch)
        }
        Value::String(_) | Value::Number(_) => Err(pager::core_error(format!(
            "Error while reading \"{key}\": Expected integer, got something else."
        ))),
        _ => Err(MethodError::Unsupported("unusual epochs")),
    }
}

/// The other parameters, which only take the values ported.
fn check_unported(request: &ApiRequest<'_>) -> Result<(), MethodError> {
    match request.value("queryKey") {
        None | Some(Value::Null) => {}
        Some(Value::String(key)) if key.is_empty() => {}
        Some(_) => return Err(MethodError::Unsupported("saved revision queries")),
    }
    Ok(())
}

/// The builtin orders of `DifferentialRevisionQuery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Order {
    /// By decreasing id.
    Newest,
    /// By increasing id.
    Oldest,
    /// By decreasing `dateModified`, then id.
    Updated,
    /// By increasing `dateModified`, then id.
    Outdated,
}

impl Order {
    /// The `order` parameter, by key or alias. Order vectors and the
    /// fulltext `relevance` are left to PHP.
    pub(crate) fn parse(value: Option<&Value>) -> Result<Self, MethodError> {
        let key = match value {
            None | Some(Value::Null) => return Ok(Self::Newest),
            Some(Value::String(key)) => key.as_str(),
            Some(_) => return Err(MethodError::Unsupported("order vectors")),
        };
        match key {
            "newest" | "created" | "order-created" => Ok(Self::Newest),
            "oldest" => Ok(Self::Oldest),
            "updated" | "order-modified" => Ok(Self::Updated),
            "outdated" => Ok(Self::Outdated),
            "relevance" => Err(MethodError::Unsupported("the relevance order")),
            other => Err(pager::core_error(format!(
                "Query \"{QUERY_CLASS}\" does not support a builtin order \"{other}\". \
                 Supported orders are: newest, created, order-created, oldest, relevance, \
                 updated, order-modified, outdated."
            ))),
        }
    }

    /// Whether the order goes by `dateModified` first.
    fn by_date(self) -> bool {
        matches!(self, Self::Updated | Self::Outdated)
    }

    /// Whether going forward means decreasing values.
    fn descending(self) -> bool {
        matches!(self, Self::Newest | Self::Updated)
    }
}

/// The attachments asked for, in the order of the request:
/// `array_select_keys()` keeps the keys which are attachments, whatever
/// their value.
fn requested_attachments(value: Option<&Value>) -> Result<Vec<&'static str>, MethodError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(list)) if list.is_empty() => Ok(Vec::new()),
        Some(Value::Object(map)) => Ok(map
            .keys()
            .filter_map(|key| ATTACHMENTS.iter().find(|known| **known == key))
            .copied()
            .collect()),
        Some(_) => Err(MethodError::Unsupported("attachments which are not a map")),
    }
}

/// A `differential_reviewer` row, with what the attachments show.
pub(crate) struct ReviewerRow {
    pub(crate) reviewer: String,
    pub(crate) status: String,
    actor: Option<String>,
    voided: Option<String>,
    last_action_diff: Option<String>,
}

impl ReviewerRow {
    /// The row of a new reviewer, which has acted on nothing yet.
    pub(crate) fn new(reviewer: String, status: String) -> Self {
        Self {
            reviewer,
            status,
            actor: None,
            voided: None,
            last_action_diff: None,
        }
    }
}

/// The reviewers of each revision, in the order `loadReviewers()` reads
/// them.
pub(crate) async fn load_reviewers(
    database: &Database,
    revisions: &[Revision],
) -> Result<HashMap<Phid, Vec<ReviewerRow>>, MethodError> {
    let mut reviewers: HashMap<Phid, Vec<ReviewerRow>> = HashMap::new();
    if revisions.is_empty() {
        return Ok(reviewers);
    }
    let sql = format!(
        "SELECT revisionPHID, reviewerPHID, reviewerStatus, lastActorPHID, voidedPHID, \
           lastActionDiffPHID FROM {} WHERE revisionPHID IN ({}) ORDER BY id ASC",
        database.table("differential", "differential_reviewer"),
        phab_db::placeholders(revisions.len())
    );
    let rows = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        revisions.iter().map(|revision| revision.phid.as_str()),
    )
    .fetch_all(database.pool())
    .await?;
    for row in &rows {
        let list = reviewers
            .entry(Phid::new(rows::text(row, "revisionPHID")?))
            .or_default();
        let reviewer = rows::text(row, "reviewerPHID")?;
        let entry = ReviewerRow {
            status: rows::text(row, "reviewerStatus")?,
            actor: rows::opt_text(row, "lastActorPHID")?,
            voided: rows::opt_text(row, "voidedPHID")?,
            last_action_diff: rows::opt_text(row, "lastActionDiffPHID")?,
            reviewer: reviewer.clone(),
        };
        // mpull() keyed by reviewer: a later row replaces an earlier one.
        match list.iter_mut().find(|known| known.reviewer == reviewer) {
            Some(known) => *known = entry,
            None => list.push(entry),
        }
    }
    Ok(reviewers)
}

/// The destinations of one edge type from each revision, newest first.
pub(crate) async fn load_edge_lists(
    database: &Database,
    revisions: &[Revision],
    edge_type: i64,
) -> Result<HashMap<Phid, Vec<Phid>>, MethodError> {
    let sources: Vec<Phid> = revisions
        .iter()
        .map(|revision| revision.phid.clone())
        .collect();
    let mut lists: HashMap<Phid, Vec<Phid>> = HashMap::new();
    for edge in phab_db::edges::load_edges(database, &sources, &[edge_type], None).await? {
        let list = lists.entry(edge.src).or_default();
        if !list.contains(&edge.dst) {
            list.push(edge.dst);
        }
    }
    Ok(lists)
}

/// What the attachments of a page show.
#[derive(Default)]
struct AttachmentData {
    reviewers: HashMap<Phid, Vec<ReviewerRow>>,
    subscribers: HashMap<Phid, Vec<Phid>>,
    projects: HashMap<Phid, Vec<Phid>>,
}

impl AttachmentData {
    async fn load(
        database: &Database,
        revisions: &[Revision],
        requested: &[&str],
    ) -> Result<Self, MethodError> {
        let mut data = Self::default();
        if requested.contains(&"reviewers") || requested.contains(&"reviewers-extra") {
            data.reviewers = load_reviewers(database, revisions).await?;
        }
        if requested.contains(&"subscribers") {
            data.subscribers =
                load_edge_lists(database, revisions, phab_db::edges::OBJECT_HAS_SUBSCRIBER).await?;
        }
        if requested.contains(&"projects") {
            data.projects =
                load_edge_lists(database, revisions, phab_db::edges::OBJECT_HAS_PROJECT).await?;
        }
        Ok(data)
    }

    /// `getAttachmentForObject()` of each attachment asked for.
    fn for_revision(
        &self,
        revision: &Revision,
        requested: &[&str],
        viewer: Option<&Phid>,
    ) -> Value {
        let reviewers = self
            .reviewers
            .get(&revision.phid)
            .map_or(&[][..], Vec::as_slice);
        let mut attachments = Map::new();
        for key in requested {
            let value = match *key {
                "reviewers" => {
                    let list: Vec<Value> = reviewers
                        .iter()
                        .map(|reviewer| {
                            json!({
                                "reviewerPHID": reviewer.reviewer,
                                "status": reviewer.status,
                                "isBlocking": reviewer.status == "blocking",
                                "actorPHID": reviewer.actor,
                            })
                        })
                        .collect();
                    json!({ "reviewers": list })
                }
                "reviewers-extra" => {
                    let list: Vec<Value> = reviewers
                        .iter()
                        .map(|reviewer| {
                            json!({
                                "reviewerPHID": reviewer.reviewer,
                                "voidedPHID": reviewer.voided,
                                "diffPHID": reviewer.last_action_diff,
                            })
                        })
                        .collect();
                    json!({ "reviewers-extra": list })
                }
                "subscribers" => {
                    let subscribers = self
                        .subscribers
                        .get(&revision.phid)
                        .map_or(&[][..], Vec::as_slice);
                    // isAutomaticallySubscribed(): the author and the
                    // reviewers who didn't resign are.
                    let viewer_is_subscribed = viewer.is_some_and(|viewer| {
                        subscribers.contains(viewer)
                            || revision.subject().implicit_subscribers.contains(viewer)
                    });
                    let shown: Vec<&str> = subscribers.iter().take(10).map(Phid::as_str).collect();
                    json!({
                        "subscriberPHIDs": shown,
                        "subscriberCount": subscribers.len(),
                        "viewerIsSubscribed": viewer_is_subscribed,
                    })
                }
                _ => {
                    let projects: Vec<&str> = self
                        .projects
                        .get(&revision.phid)
                        .map_or(&[][..], Vec::as_slice)
                        .iter()
                        .map(Phid::as_str)
                        .collect();
                    json!({ "projectPHIDs": projects })
                }
            };
            attachments.insert((*key).to_owned(), value);
        }
        Value::Object(attachments)
    }
}

/// `DifferentialRevisionQuery` with the ported constraints and orders.
pub(crate) struct Revisions<'a> {
    pub(crate) request: &'a ApiRequest<'a>,
    pub(crate) constraints: Constraints,
    pub(crate) order: Order,
    /// What the policy filter keeps between pages; the cursor's own query
    /// has a filter of its own.
    pub(crate) memory: std::sync::Mutex<phab_policy::QueryMemory>,
}

impl Revisions<'_> {
    /// The `dateModified` of a revision the query paged past.
    async fn date_modified(&self, id: u64) -> Result<i64, MethodError> {
        let database = &self.request.context.database;
        let sql = format!(
            "SELECT dateModified FROM {} WHERE id = ?",
            database.table("differential", "differential_revision")
        );
        let row = phab_db::sqlx::query(&sql)
            .bind(id)
            .fetch_optional(database.pool())
            .await?
            .ok_or_else(|| MethodError::internal("the cursor's revision vanished"))?;
        Ok(rows::int(&row, "dateModified")?)
    }
}

impl PolicyAwareQuery for Revisions<'_> {
    type Row = Revision;

    fn query_class(&self) -> &'static str {
        QUERY_CLASS
    }

    fn id(row: &Revision) -> u64 {
        u64::try_from(row.id).unwrap_or_default()
    }

    const CURSOR_RAISES_POLICY_EXCEPTIONS: bool = true;

    async fn load(
        &self,
        ids: Option<&[u64]>,
        after: Option<u64>,
        direction: Direction,
        limit: Option<usize>,
    ) -> Result<Vec<Revision>, MethodError> {
        let database = &self.request.context.database;
        let mut conditions = Vec::new();
        let mut binds: Vec<Value> = Vec::new();
        // A cursor's own query has none of the constraints.
        let constraints = if ids.is_some() {
            Constraints::default()
        } else {
            self.constraints.clone()
        };
        if let Some(ids) = ids {
            conditions.push(format!("id IN ({})", phab_db::placeholders(ids.len())));
            binds.extend(ids.iter().map(|id| json!(id)));
        }
        constraints.add_conditions(database, &mut conditions, &mut binds);
        // Paging backwards reverses the order and the comparisons.
        let descending = self.order.descending() == (direction == Direction::Forward);
        let (comparison, order) = if descending {
            ("<", "DESC")
        } else {
            (">", "ASC")
        };
        if let Some(after) = after {
            if self.order.by_date() {
                // The paging map of the row after which the page starts.
                let date = self.date_modified(after).await?;
                conditions.push(format!(
                    "(dateModified {comparison} ? OR (dateModified = ? AND id {comparison} ?))"
                ));
                binds.extend([json!(date), json!(date), json!(after)]);
            } else {
                conditions.push(format!("id {comparison} ?"));
                binds.push(json!(after));
            }
        }
        let mut sql = format!(
            "SELECT id FROM {}",
            database.table("differential", "differential_revision")
        );
        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }
        if self.order.by_date() {
            let _ = write!(sql, " ORDER BY dateModified {order}, id {order}");
        } else {
            let _ = write!(sql, " ORDER BY id {order}");
        }
        if let Some(limit) = limit {
            let _ = write!(sql, " LIMIT {limit}");
        }
        let mut query = phab_db::sqlx::query(&sql);
        for bind in &binds {
            query = match bind {
                Value::String(text) => query.bind(text.as_str()),
                other => query.bind(other.as_i64()),
            };
        }
        let rows = query.fetch_all(database.pool()).await?;
        let page: Vec<i64> = rows
            .iter()
            .map(|row| rows::int(row, "id"))
            .collect::<Result<_, _>>()?;
        let mut loaded: HashMap<i64, Revision> = storage::load_revisions_by_id(database, &page)
            .await?
            .into_iter()
            .map(|revision| (revision.id, revision))
            .collect();
        Ok(page.iter().filter_map(|id| loaded.remove(id)).collect())
    }

    async fn filter_cursor(&self, rows: Vec<Revision>) -> Result<Vec<Revision>, MethodError> {
        let context = self.request.context;
        storage::visible_revisions(
            &context.database,
            &context.config,
            &self.request.viewer,
            rows,
            self.request.now,
        )
        .await
        .map_err(MethodError::internal)
    }

    async fn filter(&self, rows: Vec<Revision>) -> Result<Vec<Revision>, MethodError> {
        let context = self.request.context;
        let mut memory = self
            .memory
            .lock()
            .map_err(|_| MethodError::internal("policy memory poisoned"))?
            .clone();
        let visible = storage::visible_revisions_in_query(
            &context.database,
            &context.config,
            &self.request.viewer,
            rows,
            self.request.now,
            &mut memory,
        )
        .await
        .map_err(MethodError::internal)?;
        *self
            .memory
            .lock()
            .map_err(|_| MethodError::internal("policy memory poisoned"))? = memory;
        Ok(visible)
    }
}

/// What the result fields need beyond [`Revision`].
pub(crate) struct Extra {
    pub(crate) summary: String,
    pub(crate) test_plan: String,
    pub(crate) date_created: i64,
    pub(crate) properties: Value,
}

pub(crate) async fn load_extra(
    database: &Database,
    revisions: &[Revision],
) -> Result<HashMap<i64, Extra>, MethodError> {
    if revisions.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT id, summary, testPlan, dateCreated, properties FROM {} WHERE id IN ({})",
        database.table("differential", "differential_revision"),
        phab_db::placeholders(revisions.len())
    );
    let rows = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        revisions.iter().map(|revision| revision.id),
    )
    .fetch_all(database.pool())
    .await?;
    rows.iter()
        .map(|row| {
            let properties = rows::text(row, "properties")?;
            Ok((
                rows::int(row, "id")?,
                Extra {
                    summary: rows::text(row, "summary")?,
                    test_plan: rows::text(row, "testPlan")?,
                    date_created: rows::int(row, "dateCreated")?,
                    properties: serde_json::from_str(&properties).unwrap_or(Value::Null),
                },
            ))
        })
        .collect::<Result<_, phab_db::DbError>>()
        .map_err(Into::into)
}

/// The IDs of the active diffs, by PHID, as strings: PHP returns the
/// column as it reads it.
async fn load_diff_ids(
    database: &Database,
    revisions: &[Revision],
) -> Result<HashMap<Phid, String>, MethodError> {
    let phids: Vec<&str> = revisions
        .iter()
        .filter_map(|revision| revision.active_diff_phid.as_ref())
        .map(Phid::as_str)
        .collect();
    if phids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT id, phid FROM {} WHERE phid IN ({})",
        database.table("differential", "differential_diff"),
        phab_db::placeholders(phids.len())
    );
    let rows = phab_db::bind_all(phab_db::sqlx::query(&sql), phids)
        .fetch_all(database.pool())
        .await?;
    rows.iter()
        .map(|row| {
            Ok((
                Phid::new(rows::text(row, "phid")?),
                rows::int(row, "id")?.to_string(),
            ))
        })
        .collect::<Result<_, phab_db::DbError>>()
        .map_err(Into::into)
}

/// The stored values of the two custom fields, by revision PHID.
pub(crate) async fn load_custom_fields(
    database: &Database,
    revisions: &[Revision],
) -> Result<HashMap<(Phid, &'static str), String>, MethodError> {
    if revisions.is_empty() {
        return Ok(HashMap::new());
    }
    let indexes: Vec<(String, &'static str)> = [BUG_FIELD, UPLIFT_FIELD]
        .into_iter()
        .map(|key| (phab_core::hash::digest_for_index(key.as_bytes()), key))
        .collect();
    let sql = format!(
        "SELECT objectPHID, fieldIndex, fieldValue FROM {} \
         WHERE objectPHID IN ({}) AND fieldIndex IN (?, ?)",
        database.table("differential", "differential_customfieldstorage"),
        phab_db::placeholders(revisions.len())
    );
    let query = phab_db::bind_all(
        phab_db::sqlx::query(&sql),
        revisions.iter().map(|revision| revision.phid.as_str()),
    );
    let query = phab_db::bind_all(query, indexes.iter().map(|(index, _)| index.as_str()));
    let rows = query.fetch_all(database.pool()).await?;
    let mut values = HashMap::new();
    for row in &rows {
        let index = rows::text(row, "fieldIndex")?;
        if let Some((_, key)) = indexes.iter().find(|(known, _)| *known == index) {
            values.insert(
                (Phid::new(rows::text(row, "objectPHID")?), *key),
                rows::text(row, "fieldValue")?,
            );
        }
    }
    Ok(values)
}

/// The walk of a revision graph from one seed.
struct Walk {
    /// The nodes reached, in the order they were loaded.
    order: Vec<Phid>,
    reached: HashSet<Phid>,
    /// The nodes to load next.
    level: Vec<Phid>,
}

/// `stackGraph`: `DifferentialRevisionGraph` loaded whole from each
/// revision, as the "depends on" edges of every revision it reaches.
/// Edges are not filtered by policy, as in PHP.
async fn load_stack_graphs(
    database: &Database,
    seeds: &[Phid],
) -> Result<HashMap<Phid, Map<String, Value>>, MethodError> {
    // The edges of each node, both ways, newest first.
    let mut known: HashMap<Phid, (Vec<Phid>, Vec<Phid>)> = HashMap::new();
    // AbstractDirectedGraph::loadGraph(), from every seed at once: each
    // keeps its own order of nodes, loaded level by level.
    let mut walks: Vec<Walk> = seeds
        .iter()
        .map(|seed| Walk {
            order: Vec::new(),
            reached: std::iter::once(seed.clone()).collect(),
            level: vec![seed.clone()],
        })
        .collect();
    while walks.iter().any(|walk| !walk.level.is_empty()) {
        let mut missing: Vec<Phid> = walks
            .iter()
            .flat_map(|walk| walk.level.iter())
            .filter(|node| !known.contains_key(*node))
            .cloned()
            .collect();
        missing.sort();
        missing.dedup();
        if !missing.is_empty() {
            let edges = phab_db::edges::load_edges(
                database,
                &missing,
                &[
                    phab_db::edges::REVISION_DEPENDS_ON_REVISION,
                    phab_db::edges::REVISION_DEPENDED_ON_BY_REVISION,
                ],
                None,
            )
            .await?;
            for node in missing {
                let of_type = |edge_type: i64| {
                    let mut destinations: Vec<Phid> = Vec::new();
                    for edge in edges
                        .iter()
                        .filter(|edge| edge.src == node && edge.edge_type == edge_type)
                    {
                        if !destinations.contains(&edge.dst) {
                            destinations.push(edge.dst.clone());
                        }
                    }
                    destinations
                };
                let parents = of_type(phab_db::edges::REVISION_DEPENDS_ON_REVISION);
                let children = of_type(phab_db::edges::REVISION_DEPENDED_ON_BY_REVISION);
                known.insert(node, (parents, children));
            }
        }
        for walk in &mut walks {
            let level = std::mem::take(&mut walk.level);
            for node in &level {
                let (parents, children) = &known[node];
                for next in parents.iter().chain(children) {
                    if walk.reached.insert(next.clone()) {
                        walk.level.push(next.clone());
                    }
                }
            }
            walk.order.extend(level);
        }
    }
    Ok(seeds
        .iter()
        .zip(walks)
        .map(|(seed, walk)| {
            let graph = walk
                .order
                .into_iter()
                .map(|node| {
                    let parents: Vec<Value> = known[&node]
                        .0
                        .iter()
                        .map(|parent| json!(parent.as_str()))
                        .collect();
                    (node.to_string(), Value::Array(parents))
                })
                .collect();
            (seed.clone(), graph)
        })
        .collect())
}

/// `DifferentialRevisionStatus`: the name, whether it is closed, and the
/// ANSI color, of a status key.
pub(crate) fn status_info(status: &str, close_on_accept: bool) -> Option<Value> {
    let (name, closed, color) = match status {
        "needs-review" => ("Needs Review", false, Some("magenta")),
        "needs-revision" => ("Needs Revision", false, Some("red")),
        "changes-planned" => ("Changes Planned", false, Some("red")),
        "accepted" => ("Accepted", close_on_accept, Some("green")),
        "published" => ("Closed", true, Some("cyan")),
        "abandoned" => ("Abandoned", true, None),
        "draft" => ("Draft", false, None),
        _ => return None,
    };
    Some(json!({"value": status, "name": name, "closed": closed, "color.ansi": color}))
}

/// `DifferentialUpliftRequestCustomField::setValueFromStorage()`: the JSON
/// object or list, anything else (no value, invalid JSON, a scalar) being
/// an empty list.
pub(crate) fn uplift_request(stored: Option<&String>) -> Value {
    match stored.and_then(|stored| serde_json::from_str::<Value>(stored).ok()) {
        Some(value @ (Value::Object(_) | Value::Array(_))) => phab_core::php::round_trip(&value),
        _ => json!([]),
    }
}

/// A property of `properties`, PHP's `getProperty($key, $default)`.
fn property(properties: &Value, key: &str) -> Option<Value> {
    properties.get(key).cloned()
}

/// What the results of a page show beyond their rows.
struct PageData<'a> {
    request: &'a ApiRequest<'a>,
    requested: &'a [&'static str],
    extra: HashMap<i64, Extra>,
    diffs: HashMap<Phid, String>,
    custom: HashMap<(Phid, &'static str), String>,
    /// `merge.conflict.status`, when the field is enabled.
    merge_conflict: Option<HashMap<Phid, Value>>,
    graphs: HashMap<Phid, Map<String, Value>>,
    attachments: AttachmentData,
    base_uri: String,
    close_on_accept: bool,
}

impl<'a> PageData<'a> {
    async fn load(
        request: &'a ApiRequest<'a>,
        revisions: &[Revision],
        requested: &'a [&'static str],
    ) -> Result<Self, MethodError> {
        let context = request.context;
        let database = &context.database;
        let seeds: Vec<Phid> = revisions
            .iter()
            .map(|revision| revision.phid.clone())
            .collect();
        Ok(Self {
            request,
            requested,
            extra: load_extra(database, revisions).await?,
            diffs: load_diff_ids(database, revisions).await?,
            custom: load_custom_fields(database, revisions).await?,
            merge_conflict: merge_conflict::conduit_values(request, revisions).await?,
            graphs: load_stack_graphs(database, &seeds).await?,
            attachments: AttachmentData::load(database, revisions, requested).await?,
            base_uri: context
                .config
                .nonempty_string("phabricator.base-uri")
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_owned(),
            close_on_accept: context.config.truthy("differential.close-on-accept"),
        })
    }

    /// One result: its fields (`getFieldValuesForConduit()` and the
    /// extensions') and attachments.
    fn result(&self, revision: &Revision) -> Result<Value, MethodError> {
        let extra = self
            .extra
            .get(&revision.id)
            .ok_or_else(|| MethodError::internal("revision row vanished"))?;
        let status = status_info(&revision.status, self.close_on_accept).ok_or(
            MethodError::Unsupported("a revision with an unknown status"),
        )?;
        // loadActiveDiff()->getID() fails without a diff.
        let diff_id = revision
            .active_diff_phid
            .as_ref()
            .and_then(|phid| self.diffs.get(phid))
            .ok_or(MethodError::Unsupported("a revision without its diff"))?;
        let should_broadcast = property(&extra.properties, "draft.broadcast")
            .is_none_or(|value| phab_core::php::is_truthy(&value));
        let hold_as_draft = property(&extra.properties, "draft.hold")
            .is_some_and(|value| phab_core::php::is_truthy(&value));
        let custom = |key| self.custom.get(&(revision.phid.clone(), key));
        let mut fields = json!({
            "stackGraph": self.graphs.get(&revision.phid).cloned().unwrap_or_default(),
            "title": revision.title,
            "uri": format!("{}/D{}", self.base_uri, revision.id),
            "authorPHID": revision.author_phid.as_ref().map(Phid::as_str),
            "status": status,
            "repositoryPHID": revision.repository_phid.as_ref().map(Phid::as_str),
            "diffPHID": revision.active_diff_phid.as_ref().map(Phid::as_str),
            "diffID": diff_id,
            "summary": extra.summary,
            "testPlan": extra.test_plan,
            "isDraft": !should_broadcast,
            "holdAsDraft": hold_as_draft,
            "dateCreated": extra.date_created,
            "dateModified": revision.date_modified,
            "policy": {"view": revision.view_policy, "edit": revision.edit_policy},
            "bugzilla.bug-id": custom(BUG_FIELD),
        });
        // The custom fields come in the order of the class map.
        if let Value::Object(map) = &mut fields {
            if let Some(values) = &self.merge_conflict {
                map.insert(
                    merge_conflict::CONDUIT_KEY.to_owned(),
                    values.get(&revision.phid).cloned().unwrap_or_default(),
                );
            }
            map.insert(
                "uplift.request".to_owned(),
                uplift_request(custom(UPLIFT_FIELD)),
            );
            // DifferentialAIChangeSearchEngineExtension, after the custom
            // fields (9000) and Mozilla's reviewers extension (9001).
            let ai_change = AiChange::from_properties(&extra.properties)?;
            map.insert(
                ai_change::CONDUIT_KEY.to_owned(),
                ai_change.conduit_value(revision.active_diff_phid.as_ref().map(Phid::as_str)),
            );
        }
        Ok(json!({
            "id": revision.id,
            "type": "DREV",
            "phid": revision.phid.as_str(),
            "fields": fields,
            "attachments": self.attachments.for_revision(
                revision,
                self.requested,
                self.request.viewer.phid.as_ref(),
            ),
        }))
    }
}

/// `differential.revision.search`.
pub struct RevisionSearch;

impl Method for RevisionSearch {
    fn name(&self) -> &'static str {
        "differential.revision.search"
    }

    fn params(&self) -> &'static [&'static str] {
        &[
            "queryKey",
            "constraints",
            "attachments",
            "order",
            "before",
            "after",
            "limit",
        ]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            // In the order buildConduitResponse() reads them.
            check_unported(request)?;
            let mut constraints = Constraints::parse(request.value("constraints"))?;
            let requested = requested_attachments(request.value("attachments"))?;
            let order = Order::parse(request.value("order"))?;
            let paging = Paging::for_search(
                request.value("limit"),
                request.value("after"),
                request.value("before"),
            )?;
            constraints.resolve(request).await?;
            let (start, direction) = paging.start();
            let query = Revisions {
                request,
                constraints,
                order,
                memory: std::sync::Mutex::default(),
            };
            let found = pager::execute(&query, start, direction, paging.size.value + 1).await?;
            let page = paging.pager().slice(found);
            let page_data = PageData::load(request, &page.items, &requested).await?;
            let data = page
                .items
                .iter()
                .map(|revision| page_data.result(revision))
                .collect::<Result<Vec<_>, _>>()?;
            let cursor =
                |revision: Option<Revision>| revision.map(|revision| revision.id.to_string());
            Ok(json!({
                "data": data,
                "maps": {},
                "query": {"queryKey": null},
                "cursor": {
                    "limit": paging.size.shown,
                    "after": cursor(page.next),
                    "before": cursor(page.previous),
                    "order": request.value("order").cloned().unwrap_or(Value::Null),
                },
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constraints_follow_php() {
        assert_eq!(Constraints::parse(None), Ok(Constraints::default()));
        assert_eq!(
            Constraints::parse(Some(&json!([]))),
            Ok(Constraints::default())
        );
        assert_eq!(
            Constraints::parse(Some(&json!({"ids": [3, 1], "phids": ["x"]}))),
            Ok(Constraints {
                ids: Some(vec![3, 1]),
                phids: Some(vec!["x".to_owned()]),
                ..Constraints::default()
            })
        );
        let error = |message: &str| Err(pager::core_error(message.to_owned()));
        for (constraints, message) in [
            (
                json!({"ids": [1], "nope": 1}),
                "Constraint \"nope\" is not a valid constraint for this query.",
            ),
            (
                json!(["x"]),
                "Constraint \"0\" is not a valid constraint for this query.",
            ),
            (
                json!({"ids": []}),
                "Error while reading \"ids\": Expected a nonempty list, but value is an empty \
                 list.",
            ),
            (
                json!({"phids": []}),
                "Error while reading \"phids\": Expected a nonempty list, but value is an \
                 empty list.",
            ),
            (
                json!({"ids": [1, "2"]}),
                "Error while reading \"ids[1]\": Expected integer, got something else.",
            ),
            (
                json!({"ids": [2.0]}),
                "Error while reading \"ids[0]\": Expected integer, got something else.",
            ),
            (
                json!({"ids": null}),
                "Error while reading \"ids\": Expected a list, but value is not a list.",
            ),
            (
                json!({"ids": {"a": 2}}),
                "Error while reading \"ids\": Expected a list, but value is an object.",
            ),
        ] {
            assert_eq!(
                Constraints::parse(Some(&constraints)),
                error(message),
                "{constraints}"
            );
        }
        for unported in [
            json!({"statuses": ["open()"]}),
            json!({"authorPHIDs": ["alice"]}),
            json!({"responsiblePHIDs": ["viewer()"]}),
            json!({"reviewerPHIDs": ["none()"]}),
            json!({"projects": ["release-managers"]}),
            json!({"subscribers": ["alice"]}),
            json!({"ids": {"0": 1}}),
            json!({"phids": [1]}),
            json!("x"),
        ] {
            assert!(
                matches!(
                    Constraints::parse(Some(&unported)),
                    Err(MethodError::Unsupported(_))
                ),
                "{unported}"
            );
        }
    }

    #[test]
    fn column_constraints_follow_php() {
        let error = |message: &str| Err(pager::core_error(message.to_owned()));
        assert_eq!(
            Constraints::parse(Some(&json!({
                "authorPHIDs": [],
                "statuses": ["accepted"],
                "repositoryPHIDs": ["PHID-REPO-1"],
                "createdStart": 5,
                "modifiedEnd": 9,
            }))),
            Ok(Constraints {
                statuses: Some(vec!["accepted".to_owned()]),
                repositories: Some(vec!["PHID-REPO-1".to_owned()]),
                created: (Some(5), None),
                modified: (None, Some(9)),
                ..Constraints::default()
            })
        );
        for (constraints, message) in [
            (
                json!({"createdStart": 0}),
                "Error while reading \"createdStart\": Epoch timestamp must be larger than 0, \
                 got 0.",
            ),
            (
                json!({"modifiedEnd": "1"}),
                "Error while reading \"modifiedEnd\": Expected integer, got something else.",
            ),
            (
                json!({"statuses": []}),
                "Error while reading \"statuses\": Expected a nonempty list, but value is an \
                 empty list.",
            ),
        ] {
            assert_eq!(
                Constraints::parse(Some(&constraints)),
                error(message),
                "{constraints}"
            );
        }
    }

    #[test]
    fn orders_follow_php() {
        assert_eq!(Order::parse(None), Ok(Order::Newest));
        assert_eq!(Order::parse(Some(&json!("created"))), Ok(Order::Newest));
        assert_eq!(
            Order::parse(Some(&json!("order-modified"))),
            Ok(Order::Updated)
        );
        assert_eq!(Order::parse(Some(&json!("outdated"))), Ok(Order::Outdated));
        assert_eq!(
            Order::parse(Some(&json!(""))),
            Err(pager::core_error(
                "Query \"DifferentialRevisionQuery\" does not support a builtin order \"\". \
                 Supported orders are: newest, created, order-created, oldest, relevance, \
                 updated, order-modified, outdated."
                    .to_owned()
            ))
        );
        assert!(matches!(
            Order::parse(Some(&json!(["id"]))),
            Err(MethodError::Unsupported(_))
        ));
        assert!(matches!(
            Order::parse(Some(&json!("relevance"))),
            Err(MethodError::Unsupported(_))
        ));
    }

    #[test]
    fn attachments_are_the_keys_asked_for() {
        assert_eq!(requested_attachments(None), Ok(vec![]));
        assert_eq!(requested_attachments(Some(&json!([]))), Ok(vec![]));
        assert_eq!(
            requested_attachments(Some(&json!({"projects": false, "nope": 1, "reviewers": 1}))),
            Ok(vec!["projects", "reviewers"])
        );
        assert!(requested_attachments(Some(&json!("reviewers"))).is_err());
    }

    #[test]
    fn statuses_and_custom_fields_like_php() {
        assert_eq!(
            status_info("accepted", true),
            Some(
                json!({"value": "accepted", "name": "Accepted", "closed": true, "color.ansi": "green"})
            )
        );
        assert_eq!(
            status_info("abandoned", false),
            Some(
                json!({"value": "abandoned", "name": "Abandoned", "closed": true, "color.ansi": null})
            )
        );
        assert_eq!(status_info("weird", false), None);
        assert_eq!(uplift_request(None), json!([]));
        assert_eq!(uplift_request(Some(&"{}".to_owned())), json!([]));
        assert_eq!(uplift_request(Some(&"null".to_owned())), json!([]));
        assert_eq!(uplift_request(Some(&"not json".to_owned())), json!([]));
        assert_eq!(
            uplift_request(Some(&r#"{"Risk": "low", "0": "x"}"#.to_owned())),
            json!({"Risk": "low", "0": "x"})
        );
    }
}
