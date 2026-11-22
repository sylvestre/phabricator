//! What the Harbormaster pages of the web interface read (`phab-web`):
//! the objects the viewer of a request can see, through the queries of the
//! search methods, and the rows those pages show which no method returns
//! (build messages, lint messages, log chunks, transactions, the Herald
//! rules running a plan).

use std::collections::{HashMap, HashSet};
use std::io::Read;

use phab_conduit::pager::Paging;
use phab_conduit::search::{self, Clauses};
use phab_conduit::{ApiRequest, MethodError};
use phab_core::Phid;
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use phab_policy::loader::NoExtendedObjects;
use phab_policy::{AutomaticCapability, Capability, ObjectKind, PolicySubject};
use serde_json::{Map, Value, json};

use crate::query::{Order, Query};
use crate::storage::{self, Object};
pub use crate::storage::{Artifact, Build, Buildable, Log, Plan, Policies, Step, Target};

/// `HarbormasterBuildTarget::getName()`.
pub fn target_name(target: &Target) -> String {
    crate::target::target_name(target)
}

/// `HarbormasterBuildStep::getName()`: its name, or its implementation's;
/// `None` for a step without a name whose implementation is unknown,
/// which PHP can't show.
pub fn step_name(step: &Step) -> Option<String> {
    match &step.name {
        Some(name) if !name.is_empty() => Some(name.clone()),
        _ => crate::step_implementation(&step.class_name).map(|(name, _)| name.to_owned()),
    }
}

/// `HarbormasterBuildStepImplementation::getName()` of a class, if it is
/// one of this install's.
pub fn implementation_name(class: &str) -> Option<&'static str> {
    crate::step_implementation(class).map(|(name, _)| name)
}

/// `HarbormasterBuildPlan::getName()`: autoplans are named by their
/// application.
pub fn plan_name(plan: &Plan) -> String {
    match plan.auto_key.as_deref() {
        // `HarbormasterBuildArcanistAutoplan::getAutoplanName()`.
        Some("arcanist") => "arc lint + arc unit".to_owned(),
        _ => plan.name.clone(),
    }
}

/// An object the viewer looked up by ID.
#[derive(Debug, Clone, PartialEq)]
pub enum Lookup<T> {
    /// There is no such row, or the viewer can't see what it belongs to
    /// (`willFilterPage()` drops it, and PHP answers 404).
    Missing,
    /// The row exists, and the policy filter rejects it (a policy
    /// exception).
    Denied,
    /// The viewer sees it.
    Visible(T),
}

fn ids_clause<T: Object>(ids: &[i64]) -> Clauses {
    let mut clauses = Clauses::default();
    clauses.in_list(T::ID.sql, ids.iter().map(|id| json!(id)));
    clauses
}

fn in_clause(column: &str, values: &[String]) -> Clauses {
    let mut clauses = Clauses::default();
    clauses.in_list(column, search::strings(values));
    clauses
}

/// `executeOne()` of a query by ID, without any constraint but the ID.
async fn one<T: Object>(request: &ApiRequest<'_>, id: i64) -> Result<Lookup<T>, MethodError> {
    let rows: Vec<T> = Query::new(request, ids_clause::<T>(&[id]), Order::Newest)
        .all()
        .await?;
    Ok(rows
        .into_iter()
        .next()
        .map_or(Lookup::Missing, Lookup::Visible))
}

/// The buildable `B{id}` (`HarbormasterBuildableQuery::withIDs()`).
pub async fn buildable(
    request: &ApiRequest<'_>,
    id: i64,
) -> Result<Lookup<Buildable>, MethodError> {
    one(request, id).await
}

/// The build with this ID (`HarbormasterBuildQuery::withIDs()`).
pub async fn build(request: &ApiRequest<'_>, id: i64) -> Result<Lookup<Build>, MethodError> {
    one(request, id).await
}

/// The buildables with these PHIDs the viewer can see.
pub async fn buildables_by_phid(
    request: &ApiRequest<'_>,
    phids: &[Phid],
) -> Result<Vec<Buildable>, MethodError> {
    if phids.is_empty() {
        return Ok(Vec::new());
    }
    let clauses = in_clause("buildable.phid", &storage::phid_strings(phids));
    Query::new(request, clauses, Order::Newest).all().await
}

/// The builds with these PHIDs the viewer can see.
pub async fn builds_by_phid(
    request: &ApiRequest<'_>,
    phids: &[Phid],
) -> Result<Vec<Build>, MethodError> {
    if phids.is_empty() {
        return Ok(Vec::new());
    }
    let clauses = in_clause("b.phid", &storage::phid_strings(phids));
    Query::new(request, clauses, Order::Newest).all().await
}

/// The build plan with this ID (`HarbormasterBuildPlanQuery::withIDs()`):
/// plans have policies of their own, so a plan the viewer can't see is
/// [`Lookup::Denied`].
pub async fn plan(request: &ApiRequest<'_>, id: i64) -> Result<Lookup<Plan>, MethodError> {
    let rows: Vec<Plan> =
        storage::load_where(&request.context.database, "id", &[id.to_string()]).await?;
    if rows.is_empty() {
        return Ok(Lookup::Missing);
    }
    Ok(Plan::visible(request, rows)
        .await?
        .pop()
        .map_or(Lookup::Denied, Lookup::Visible))
}

/// A page of a list, newest first, and the cursor of the next one.
#[derive(Debug, Clone, PartialEq)]
pub struct Page<T> {
    /// The rows of the page.
    pub items: Vec<T>,
    /// The `after` cursor of the next page, if there is one.
    pub next: Option<i64>,
}

async fn page<T: Object>(
    request: &ApiRequest<'_>,
    clauses: Clauses,
    size: usize,
    after: Option<i64>,
) -> Result<Page<T>, MethodError> {
    let after = after.map(|after| json!(after.to_string()));
    let paging = Paging::parse(None, after.as_ref(), None, size)?;
    let paged = Query::<T>::new(request, clauses, Order::Newest)
        .page(&paging)
        .await?;
    Ok(Page {
        items: paged.items,
        next: paged.next.map(|row| row.id()),
    })
}

/// The buildables of the "All Buildables" query
/// (`HarbormasterBuildableSearchEngine`), `size` at a time.
pub async fn buildables_page(
    request: &ApiRequest<'_>,
    size: usize,
    after: Option<i64>,
) -> Result<Page<Buildable>, MethodError> {
    page(request, Clauses::default(), size, after).await
}

/// The plans with these statuses (all of them for `None`), `size` at a
/// time (`HarbormasterBuildPlanSearchEngine`).
pub async fn plans_page(
    request: &ApiRequest<'_>,
    statuses: Option<&[String]>,
    size: usize,
    after: Option<i64>,
) -> Result<Page<Plan>, MethodError> {
    let clauses = statuses.map_or_else(Clauses::default, |statuses| {
        in_clause("plan.planStatus", statuses)
    });
    page(request, clauses, size, after).await
}

/// The builds of a plan, newest first, `size` at a time.
pub async fn builds_of_plan(
    request: &ApiRequest<'_>,
    plan: &Phid,
    size: usize,
) -> Result<Page<Build>, MethodError> {
    let clauses = in_clause("b.buildPlanPHID", &[plan.as_str().to_owned()]);
    page(request, clauses, size, None).await
}

/// The buildables of these objects, newest first
/// (`withBuildablePHIDs()`), only the automatic or manual ones if
/// `manual` says so.
pub async fn buildables_of_objects(
    request: &ApiRequest<'_>,
    objects: &[Phid],
    manual: Option<bool>,
) -> Result<Vec<Buildable>, MethodError> {
    if objects.is_empty() {
        return Ok(Vec::new());
    }
    let mut clauses = in_clause("buildable.buildablePHID", &storage::phid_strings(objects));
    if let Some(manual) = manual {
        clauses.condition(
            "buildable.isManualBuildable = ?",
            [json!(i64::from(manual))],
        );
    }
    Query::new(request, clauses, Order::Newest).all().await
}

/// The builds of these buildables, newest first.
pub async fn builds_of_buildables(
    request: &ApiRequest<'_>,
    buildables: &[Phid],
) -> Result<Vec<Build>, MethodError> {
    if buildables.is_empty() {
        return Ok(Vec::new());
    }
    let clauses = in_clause("b.buildablePHID", &storage::phid_strings(buildables));
    Query::new(request, clauses, Order::Newest).all().await
}

/// The targets of these builds, newest first, of one generation if
/// `generation` says so.
pub async fn targets_of_builds(
    request: &ApiRequest<'_>,
    builds: &[Phid],
    generation: Option<i64>,
) -> Result<Vec<Target>, MethodError> {
    if builds.is_empty() {
        return Ok(Vec::new());
    }
    let mut clauses = in_clause("target.buildPHID", &storage::phid_strings(builds));
    if let Some(generation) = generation {
        clauses.in_list("target.buildGeneration", [json!(generation)]);
    }
    Query::new(request, clauses, Order::Newest).all().await
}

/// The steps of a plan, newest first.
pub async fn steps_of_plan(
    request: &ApiRequest<'_>,
    plan: &Phid,
) -> Result<Vec<Step>, MethodError> {
    let clauses = in_clause("step.buildPlanPHID", &[plan.as_str().to_owned()]);
    Query::new(request, clauses, Order::Newest).all().await
}

/// The steps with these PHIDs the viewer can see.
pub async fn steps_by_phid(
    request: &ApiRequest<'_>,
    phids: &[String],
) -> Result<Vec<Step>, MethodError> {
    if phids.is_empty() {
        return Ok(Vec::new());
    }
    let clauses = in_clause("step.phid", phids);
    Query::new(request, clauses, Order::Newest).all().await
}

/// The artifacts of these targets, newest first.
pub async fn artifacts_of_targets(
    request: &ApiRequest<'_>,
    targets: &[Phid],
) -> Result<Vec<Artifact>, MethodError> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let clauses = in_clause("artifact.buildTargetPHID", &storage::phid_strings(targets));
    Query::new(request, clauses, Order::Newest).all().await
}

/// The logs of these targets, newest first.
pub async fn logs_of_targets(
    request: &ApiRequest<'_>,
    targets: &[Phid],
) -> Result<Vec<Log>, MethodError> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let clauses = in_clause("log.buildTargetPHID", &storage::phid_strings(targets));
    Query::new(request, clauses, Order::Newest).all().await
}

/// The plans with these PHIDs the viewer can see.
pub async fn plans_by_phid(
    request: &ApiRequest<'_>,
    phids: &[Phid],
) -> Result<HashMap<Phid, Plan>, MethodError> {
    crate::visibility::visible_plans_by_phid(request, storage::phid_strings(phids)).await
}

/// Whether the viewer can edit each of these plans
/// (`PhabricatorPolicyFilter::hasCapability()` with `CAN_EDIT`).
pub async fn can_edit_plans(
    request: &ApiRequest<'_>,
    plans: &[Plan],
) -> Result<Vec<bool>, MethodError> {
    if plans.is_empty() {
        return Ok(Vec::new());
    }
    let context = request.context;
    let most_open = crate::most_open(&context.config);
    let subjects: Vec<PolicySubject> = plans
        .iter()
        .map(|plan| {
            let policies = plan.policies(most_open);
            let mut subject = PolicySubject::new(Some(plan.phid.clone()), ObjectKind::Other)
                .with_policy(Capability::VIEW, policies.view)
                .with_policy(Capability::EDIT, policies.edit);
            subject.subscribable = true;
            subject
        })
        .collect();
    let refs: Vec<&PolicySubject> = subjects.iter().collect();
    let facts = phab_policy::loader::load_facts(
        &context.database,
        &context.config,
        &request.viewer,
        &refs,
        &NoExtendedObjects,
        request.now,
    )
    .await?;
    Ok(phab_policy::filter(
        &request.viewer,
        &refs,
        &[Capability::EDIT],
        &facts,
    )?)
}

/// A `harbormaster_buildmessage` row: a command sent to a build, or a
/// message sent to a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The row ID.
    pub id: i64,
    /// Who sent it.
    pub author_phid: Phid,
    /// The build or target it was sent to.
    pub receiver_phid: Phid,
    /// `type`, like `restart` or `pass`.
    pub kind: String,
    /// `isConsumed`.
    pub is_consumed: bool,
    /// `dateCreated`.
    pub date_created: i64,
}

/// The messages sent to these receivers, oldest first, only the
/// unconsumed ones if `unconsumed` says so. The receivers must be objects
/// the viewer sees (`HarbormasterBuildMessageQuery::willFilterPage()`).
pub async fn messages(
    database: &Database,
    receivers: &[Phid],
    unconsumed: bool,
) -> Result<Vec<Message>, DbError> {
    if receivers.is_empty() {
        return Ok(Vec::new());
    }
    let receivers = storage::phid_strings(receivers);
    let sql = format!(
        "SELECT id, authorPHID, receiverPHID, type, isConsumed, dateCreated FROM {} \
         WHERE receiverPHID IN ({}){} ORDER BY id ASC",
        database.table("harbormaster", "harbormaster_buildmessage"),
        phab_db::placeholders(receivers.len()),
        if unconsumed {
            " AND isConsumed = 0"
        } else {
            ""
        },
    );
    phab_db::bind_all(phab_db::sqlx::query(&sql), &receivers)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            Ok(Message {
                id: rows::int(row, "id")?,
                author_phid: Phid::new(rows::text(row, "authorPHID")?),
                receiver_phid: Phid::new(rows::text(row, "receiverPHID")?),
                kind: rows::text(row, "type")?,
                is_consumed: rows::flag(row, "isConsumed")?,
                date_created: rows::int(row, "dateCreated")?,
            })
        })
        .collect()
}

/// `HarbormasterBuild::getUnprocessedMessageState()`: the pending status
/// the unconsumed commands of a build (oldest first) put it in, if any.
pub fn pending_status(unconsumed: &[Message]) -> Option<&'static str> {
    let (mut restarting, mut aborting, mut pausing, mut resuming) = (false, false, false, false);
    for message in unconsumed {
        match message.kind.as_str() {
            "restart" => (restarting, aborting) = (true, false),
            "abort" => (aborting, restarting) = (true, false),
            "pause" => (pausing, resuming) = (true, false),
            "resume" => (resuming, pausing) = (true, false),
            _ => {}
        }
    }
    if restarting {
        Some("x-restarting")
    } else if aborting {
        Some("x-aborting")
    } else if pausing {
        Some("x-pausing")
    } else if resuming {
        Some("x-resuming")
    } else {
        None
    }
}

/// A `harbormaster_buildlintmessage` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintMessage {
    /// The row ID.
    pub id: i64,
    /// The file.
    pub path: String,
    /// The line, if any.
    pub line: Option<i64>,
    /// The linter's code, like `E501`.
    pub code: String,
    /// `ArcanistLintSeverity`, like `error`.
    pub severity: String,
    /// The message.
    pub name: String,
}

/// The lint messages of these targets (`loadAllWhere()`, which has no
/// policy), by ID.
pub async fn lint_messages(
    database: &Database,
    targets: &[Phid],
) -> Result<Vec<LintMessage>, DbError> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let targets = storage::phid_strings(targets);
    let sql = format!(
        "SELECT id, path, line, code, severity, name FROM {} WHERE buildTargetPHID IN ({}) \
         ORDER BY id ASC",
        database.table("harbormaster", "harbormaster_buildlintmessage"),
        phab_db::placeholders(targets.len()),
    );
    phab_db::bind_all(phab_db::sqlx::query(&sql), &targets)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            Ok(LintMessage {
                id: rows::int(row, "id")?,
                path: rows::text(row, "path")?,
                line: rows::opt_int(row, "line")?,
                code: rows::text(row, "code")?,
                severity: rows::text(row, "severity")?,
                name: rows::text(row, "name")?,
            })
        })
        .collect()
}

/// A `harbormaster_buildunitmessage` row: the result of one test.
#[derive(Debug, Clone, PartialEq)]
pub struct UnitMessage {
    /// The row ID.
    pub id: i64,
    /// The build target which reported it.
    pub target_phid: Phid,
    /// The test engine, possibly empty.
    pub engine: String,
    /// The namespace, possibly empty.
    pub namespace: String,
    /// The name, from the string table (`HarbormasterString`), or `None`
    /// when it is missing there.
    pub name: Option<String>,
    /// `nameIndex`, which PHP shows when the name is missing.
    pub name_index: Vec<u8>,
    /// `ArcanistUnitTestResult`, like `fail`.
    pub result: String,
    /// The duration, in seconds.
    pub duration: Option<f64>,
    /// The properties, like `details` and `format`.
    pub properties: Map<String, Value>,
}

/// The unit test results of these targets, newest first
/// (`HarbormasterBuildUnitMessageQuery`, whose messages everyone may see),
/// with their names (`didFilterPage()`).
pub async fn unit_messages(
    database: &Database,
    targets: &[Phid],
) -> Result<Vec<UnitMessage>, DbError> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let targets = storage::phid_strings(targets);
    let sql = format!(
        "SELECT message.id, message.buildTargetPHID, message.engine, message.namespace, \
         message.nameIndex, message.result, message.duration, message.properties, \
         string.stringValue FROM {} message LEFT JOIN {} string \
         ON string.stringIndex = message.nameIndex \
         WHERE message.buildTargetPHID IN ({}) ORDER BY message.id DESC",
        database.table("harbormaster", "harbormaster_buildunitmessage"),
        database.table("harbormaster", "harbormaster_string"),
        phab_db::placeholders(targets.len()),
    );
    phab_db::bind_all(phab_db::sqlx::query(&sql), &targets)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            let get = |column: &str| -> Result<Option<f64>, DbError> {
                phab_db::sqlx::Row::try_get(row, column).map_err(DbError::from)
            };
            let name_index: Vec<u8> =
                phab_db::sqlx::Row::try_get(row, "nameIndex").map_err(DbError::from)?;
            Ok(UnitMessage {
                id: rows::int(row, "id")?,
                target_phid: Phid::new(rows::text(row, "buildTargetPHID")?),
                engine: rows::text(row, "engine")?,
                namespace: rows::text(row, "namespace")?,
                name: rows::opt_text(row, "stringValue")?,
                name_index,
                result: rows::text(row, "result")?,
                duration: get("duration")?,
                properties: storage::json_map(row, "properties"),
            })
        })
        .collect()
}

/// Why a log can't be read.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// A chunk is in an encoding PHP doesn't know, or doesn't inflate:
    /// PHP fails too.
    #[error("log chunk {0} can't be read")]
    Chunk(i64),
}

/// `HarbormasterBuildLog::getLogText()`: the chunks of the log, in order,
/// inflated.
pub async fn log_text(database: &Database, log_id: i64) -> Result<Vec<u8>, LogError> {
    let sql = format!(
        "SELECT id, encoding, chunk FROM {} WHERE logID = ? ORDER BY id ASC",
        database.table("harbormaster", "harbormaster_buildlogchunk"),
    );
    let mut text = Vec::new();
    for row in phab_db::sqlx::query(&sql)
        .bind(log_id)
        .fetch_all(database.pool())
        .await
        .map_err(DbError::from)?
    {
        let id = rows::int(&row, "id")?;
        let chunk: Vec<u8> = phab_db::sqlx::Row::try_get(&row, "chunk").map_err(DbError::from)?;
        match rows::text(&row, "encoding")?.as_str() {
            "text" => text.extend_from_slice(&chunk),
            // `gzinflate()`: raw deflate.
            "gzip" => {
                flate2::read::DeflateDecoder::new(chunk.as_slice())
                    .read_to_end(&mut text)
                    .map_err(|_| LogError::Chunk(id))?;
            }
            _ => return Err(LogError::Chunk(id)),
        }
    }
    Ok(text)
}

/// The transactions of one Harbormaster object type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionTable {
    /// `HarbormasterBuildableTransaction`.
    Buildable,
    /// `HarbormasterBuildTransaction`.
    Build,
    /// `HarbormasterBuildPlanTransaction`.
    Plan,
    /// `HeraldRuleTransaction`, whose rows have the same columns.
    HeraldRule,
}

impl TransactionTable {
    /// The database and the table.
    fn location(self) -> (&'static str, &'static str) {
        match self {
            Self::Buildable => ("harbormaster", "harbormaster_buildabletransaction"),
            Self::Build => ("harbormaster", "harbormaster_buildtransaction"),
            Self::Plan => ("harbormaster", "harbormaster_buildplantransaction"),
            Self::HeraldRule => ("herald", "herald_ruletransaction"),
        }
    }
}

/// A transaction row.
#[derive(Debug, Clone, PartialEq)]
pub struct Transaction {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// Who made it.
    pub author_phid: Phid,
    /// The view policy.
    pub view_policy: String,
    /// The edit policy.
    pub edit_policy: String,
    /// The comment, if any.
    pub comment_phid: Option<Phid>,
    /// `transactionType`.
    pub transaction_type: String,
    /// The decoded old value.
    pub old_value: Value,
    /// The decoded new value.
    pub new_value: Value,
    /// The decoded metadata.
    pub metadata: Map<String, Value>,
    /// `dateCreated`.
    pub date_created: i64,
}

impl Transaction {
    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        let metadata = match rows::json(row, "metadata") {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        };
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            author_phid: Phid::new(rows::text(row, "authorPHID")?),
            view_policy: rows::text(row, "viewPolicy")?,
            edit_policy: rows::text(row, "editPolicy")?,
            comment_phid: rows::opt_phid(row, "commentPHID")?,
            transaction_type: rows::text(row, "transactionType")?,
            old_value: rows::json(row, "oldValue").unwrap_or(Value::Null),
            new_value: rows::json(row, "newValue").unwrap_or(Value::Null),
            metadata,
            date_created: rows::int(row, "dateCreated")?,
        })
    }

    /// `PhabricatorApplicationTransaction` as a policy object: its author
    /// can always see it.
    pub fn subject(&self) -> PolicySubject {
        let mut subject = PolicySubject::new(Some(self.phid.clone()), ObjectKind::Other)
            .with_policy(Capability::VIEW, self.view_policy.clone())
            .with_policy(Capability::EDIT, self.edit_policy.clone());
        subject.automatic.push(AutomaticCapability {
            principal: self.author_phid.clone(),
            capabilities: None,
        });
        subject
    }

    /// `getMetadataValue()` as a boolean, like `core.create`.
    pub fn metadata_flag(&self, key: &str) -> bool {
        self.metadata
            .get(key)
            .is_some_and(phab_core::php::is_truthy)
    }
}

/// The transactions of an object, oldest first.
pub async fn transactions(
    database: &Database,
    table: TransactionTable,
    object: &Phid,
) -> Result<Vec<Transaction>, DbError> {
    let sql = format!(
        "SELECT id, phid, authorPHID, viewPolicy, editPolicy, commentPHID, transactionType, \
         oldValue, newValue, metadata, dateCreated FROM {} WHERE objectPHID = ? ORDER BY id ASC",
        database.table(table.location().0, table.location().1),
    );
    phab_db::sqlx::query(&sql)
        .bind(object.as_str())
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(Transaction::from_row)
        .collect()
}

/// `HeraldRuleActionAffectsObjectEdgeType::EDGECONST`.
const RULE_AFFECTS_OBJECT: i64 = 69;

/// A Herald rule which acts on an object, like a rule running a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffectingRule {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// The name.
    pub name: String,
    /// The adapter, like `differential`.
    pub content_type: String,
    /// `global`, `personal` or `object`.
    pub rule_type: String,
    /// `isDisabled`.
    pub is_disabled: bool,
    /// The author.
    pub author_phid: Phid,
    /// The project or repository of an object rule.
    pub trigger_object_phid: Option<Phid>,
}

/// The enabled Herald rules whose actions affect `object`, newest first
/// (`HeraldRuleQuery::withAffectedObjectPHIDs()`), without any policy.
pub async fn rules_affecting(
    database: &Database,
    object: &Phid,
) -> Result<Vec<AffectingRule>, DbError> {
    let sql = format!(
        "SELECT rule.id, rule.phid, rule.name, rule.contentType, rule.ruleType, \
         rule.isDisabled, rule.authorPHID, rule.triggerObjectPHID FROM {} rule \
         JOIN {} edge_affects ON rule.phid = edge_affects.src AND edge_affects.type = ? \
         WHERE edge_affects.dst = ? AND rule.isDisabled = 0 ORDER BY rule.id DESC",
        database.table("herald", "herald_rule"),
        database.table("herald", "edge"),
    );
    phab_db::sqlx::query(&sql)
        .bind(RULE_AFFECTS_OBJECT)
        .bind(object.as_str())
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            Ok(AffectingRule {
                id: rows::int(row, "id")?,
                phid: Phid::new(rows::text(row, "phid")?),
                name: rows::text(row, "name")?,
                content_type: rows::text(row, "contentType")?,
                rule_type: rows::text(row, "ruleType")?,
                is_disabled: rows::flag(row, "isDisabled")?,
                author_phid: Phid::new(rows::text(row, "authorPHID")?),
                trigger_object_phid: rows::opt_phid(row, "triggerObjectPHID")?,
            })
        })
        .collect()
}

/// Whether `user` (or everyone) saved, reordered or disabled queries of a
/// search engine: the default query of its list page is then theirs, not
/// the first builtin one (`PhabricatorApplicationSearchEngine`).
pub async fn has_named_queries(
    database: &Database,
    user: &Phid,
    engine: &str,
) -> Result<bool, DbError> {
    let sql = format!(
        "SELECT 1 FROM {} WHERE engineClassName = ? AND userPHID IN (?, 'scope.global') LIMIT 1",
        database.table("search", "search_namedquery"),
    );
    Ok(phab_db::sqlx::query(&sql)
        .bind(engine)
        .bind(user.as_str())
        .fetch_optional(database.pool())
        .await?
        .is_some())
}

/// The revisions of these diffs, by diff, for those which have one
/// (`DifferentialDiff::getHarbormasterBuildableDisplayPHID()`).
pub async fn diff_revisions(
    database: &Database,
    diffs: &[Phid],
) -> Result<HashMap<Phid, Phid>, DbError> {
    if diffs.is_empty() {
        return Ok(HashMap::new());
    }
    let diffs = storage::phid_strings(diffs);
    let sql = format!(
        "SELECT diff.phid AS diff, revision.phid AS revision FROM {} diff JOIN {} revision \
         ON revision.id = diff.revisionID WHERE diff.phid IN ({})",
        database.table("differential", "differential_diff"),
        database.table("differential", "differential_revision"),
        phab_db::placeholders(diffs.len()),
    );
    phab_db::bind_all(phab_db::sqlx::query(&sql), &diffs)
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(|row| {
            Ok((
                Phid::new(rows::text(row, "diff")?),
                Phid::new(rows::text(row, "revision")?),
            ))
        })
        .collect()
}

/// The table holding the objects of a PHID type the pages name, as
/// `(database, table)`.
fn table_of(phid: &Phid) -> Option<(&'static str, &'static str)> {
    Some(match phid.as_str().get(5..9)? {
        "USER" => ("user", "user"),
        "PROJ" => ("project", "project"),
        "DREV" => ("differential", "differential_revision"),
        "DIFF" => ("differential", "differential_diff"),
        "CMIT" => ("repository", "repository_commit"),
        "REPO" => ("repository", "repository"),
        "HMBB" => ("harbormaster", "harbormaster_buildable"),
        "HMBD" => ("harbormaster", "harbormaster_build"),
        "HMCP" => ("harbormaster", "harbormaster_buildplan"),
        "FILE" => ("file", "file"),
        _ => return None,
    })
}

/// Which of these objects exist, whoever may see them: a handle names a
/// missing object "Unknown Object", and one the viewer can't see
/// "Restricted". `None` when one is of a type the pages don't name.
pub async fn existing(
    database: &Database,
    phids: &[Phid],
) -> Result<Option<HashSet<Phid>>, DbError> {
    let mut by_table: HashMap<(&str, &str), Vec<&str>> = HashMap::new();
    for phid in phids {
        let Some(table) = table_of(phid) else {
            return Ok(None);
        };
        by_table.entry(table).or_default().push(phid.as_str());
    }
    let mut found = HashSet::new();
    for ((application, table), phids) in by_table {
        let sql = format!(
            "SELECT phid FROM {} WHERE phid IN ({})",
            database.table(application, table),
            phab_db::placeholders(phids.len()),
        );
        for row in phab_db::bind_all(phab_db::sqlx::query(&sql), &phids)
            .fetch_all(database.pool())
            .await?
        {
            found.insert(Phid::new(rows::text(&row, "phid")?));
        }
    }
    Ok(Some(found))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(kind: &str) -> Message {
        Message {
            id: 1,
            author_phid: Phid::new("PHID-USER-a"),
            receiver_phid: Phid::new("PHID-HMBD-1"),
            kind: kind.to_owned(),
            is_consumed: false,
            date_created: 1,
        }
    }

    #[test]
    fn later_commands_win() {
        assert_eq!(pending_status(&[]), None);
        assert_eq!(
            pending_status(&[message("restart"), message("abort")]),
            Some("x-aborting")
        );
        assert_eq!(
            pending_status(&[message("pause"), message("restart")]),
            Some("x-restarting")
        );
        assert_eq!(
            pending_status(&[message("pause"), message("resume")]),
            Some("x-resuming")
        );
        assert_eq!(pending_status(&[message("pass")]), None);
    }

    #[test]
    fn autoplans_are_named_by_their_application() {
        let mut plan = Plan {
            id: 1,
            phid: Phid::new("PHID-HMCP-1"),
            name: "arcanist".to_owned(),
            status: "active".to_owned(),
            auto_key: Some("arcanist".to_owned()),
            view_policy: "users".to_owned(),
            edit_policy: "users".to_owned(),
            properties: Map::new(),
            dates: (1, 1),
        };
        assert_eq!(plan_name(&plan), "arc lint + arc unit");
        plan.auto_key = None;
        assert_eq!(plan_name(&plan), "arcanist");
    }
}
