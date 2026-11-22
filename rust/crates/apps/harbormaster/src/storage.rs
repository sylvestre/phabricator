//! The Harbormaster rows (`HarbormasterBuildable`, `HarbormasterBuild`,
//! `HarbormasterBuildPlan`, `HarbormasterBuildStep`,
//! `HarbormasterBuildTarget`, `HarbormasterBuildArtifact` and
//! `HarbormasterBuildLog`), and what their queries share.

use std::fmt::Write;
use std::future::Future;

use phab_conduit::order::Column;
use phab_conduit::search::Clauses;
use phab_conduit::{ApiRequest, MethodError};
use phab_core::Phid;
use phab_db::sqlx::mysql::MySqlRow;
use phab_db::{Database, DbError, rows};
use serde_json::{Map, Value, json};

/// The view and edit policies of an object, as `getPolicy()` returns
/// them; Harbormaster objects inherit them from another object, which
/// the query attaches once the viewer can see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policies {
    /// The view policy.
    pub view: String,
    /// The edit policy.
    pub edit: String,
}

impl Policies {
    /// The `policy` field of objects whose `getCapabilities()` are view
    /// and edit.
    pub(crate) fn view_and_edit(&self) -> Value {
        json!({"view": self.view, "edit": self.edit})
    }

    /// The `policy` field of objects which only have the view capability.
    pub(crate) fn view_only(&self) -> Value {
        json!({"view": self.view})
    }
}

/// A Harbormaster row, its table and the query which loads it.
pub(crate) trait Object: Clone + Send + Sync + Sized + 'static {
    /// The PHP query class, for error messages.
    const QUERY_CLASS: &'static str;
    /// The table, in the `harbormaster` database.
    const TABLE: &'static str;
    /// The table alias in queries (`getPrimaryTableAlias()`).
    const ALIAS: &'static str;
    /// The `id` column, with its alias.
    const ID: Column;
    /// Whether a cursor naming a row the viewer can't see raises a policy
    /// exception: rows which `willFilterPage()` keeps can still be
    /// rejected by the policy filter.
    const CURSOR_RAISES_POLICY_EXCEPTIONS: bool = false;

    /// Reads a row.
    fn from_row(row: &MySqlRow) -> Result<Self, DbError>;

    /// The row ID.
    fn id(&self) -> i64;

    /// `willFilterPage()` and the policy filter: the rows the viewer can
    /// see, in their order, with the policies they inherit attached.
    fn visible(
        request: &ApiRequest<'_>,
        rows: Vec<Self>,
    ) -> impl Future<Output = Result<Vec<Self>, MethodError>> + Send;
}

/// The rows of `T` matching `clauses`, then `tail` (`ORDER BY`...).
pub(crate) async fn select<T: Object>(
    database: &Database,
    clauses: &Clauses,
    tail: &str,
) -> Result<Vec<T>, MethodError> {
    let select = format!(
        "SELECT {alias}.* FROM {} {alias}",
        database.table("harbormaster", T::TABLE),
        alias = T::ALIAS,
    );
    let sql = clauses.sql(&select, tail);
    Ok(clauses
        .bind(phab_db::sqlx::query(&sql))
        .fetch_all(database.pool())
        .await?
        .iter()
        .map(T::from_row)
        .collect::<Result<_, _>>()?)
}

/// The rows of `T` whose `column` is one of `values`, newest first (the
/// default order of the queries), without any visibility check.
pub(crate) async fn load_where<T: Object>(
    database: &Database,
    column: &str,
    values: &[String],
) -> Result<Vec<T>, MethodError> {
    if values.is_empty() {
        return Ok(Vec::new());
    }
    let mut clauses = Clauses::default();
    clauses.in_list(
        &format!("{}.{column}", T::ALIAS),
        phab_conduit::search::strings(values),
    );
    let mut tail = String::new();
    let _ = write!(tail, " ORDER BY {} DESC", T::ID.sql);
    select(database, &clauses, &tail).await
}

/// A JSON dictionary column; Lisk decodes a JSON list (an empty PHP array)
/// as an array too, and the pages treat anything else as empty.
pub(crate) fn json_map(row: &MySqlRow, column: &str) -> Map<String, Value> {
    match rows::json(row, column) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// The PHIDs of rows, as strings.
pub(crate) fn phid_strings<'a>(phids: impl IntoIterator<Item = &'a Phid>) -> Vec<String> {
    let mut phids: Vec<String> = phids
        .into_iter()
        .map(|phid| phid.as_str().to_owned())
        .collect();
    phids.sort();
    phids.dedup();
    phids
}

/// A `harbormaster_buildable` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Buildable {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// The object built, like a diff.
    pub object_phid: Phid,
    /// The object containing it, like the revision of a diff.
    pub container_phid: Option<String>,
    /// The status constant.
    pub status: String,
    /// `isManualBuildable`.
    pub is_manual: bool,
    /// `dateCreated` and `dateModified`.
    pub dates: (i64, i64),
    /// The policies of the object built, once the viewer can see it.
    pub policies: Option<Policies>,
}

impl Object for Buildable {
    const QUERY_CLASS: &'static str = "HarbormasterBuildableQuery";
    const TABLE: &'static str = "harbormaster_buildable";
    const ALIAS: &'static str = "buildable";
    const ID: Column = Column::new("buildable.id");

    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            object_phid: Phid::new(rows::text(row, "buildablePHID")?),
            container_phid: rows::opt_text(row, "containerPHID")?,
            status: rows::text(row, "buildableStatus")?,
            is_manual: rows::flag(row, "isManualBuildable")?,
            dates: (
                rows::int(row, "dateCreated")?,
                rows::int(row, "dateModified")?,
            ),
            policies: None,
        })
    }

    fn id(&self) -> i64 {
        self.id
    }

    async fn visible(request: &ApiRequest<'_>, rows: Vec<Self>) -> Result<Vec<Self>, MethodError> {
        crate::visibility::visible_buildables(request, rows).await
    }
}

/// A `harbormaster_build` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Build {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// The buildable built.
    pub buildable_phid: Phid,
    /// The build plan.
    pub plan_phid: Phid,
    /// The status constant.
    pub status: String,
    /// `buildGeneration`: how many times it was restarted.
    pub generation: i64,
    /// Who started it, if anyone.
    pub initiator_phid: Option<String>,
    /// `planAutoKey`: the autoplan it runs, for automatic builds.
    pub plan_auto_key: Option<String>,
    /// `dateCreated` and `dateModified`.
    pub dates: (i64, i64),
    /// The policies of the buildable, once the viewer can see it.
    pub policies: Option<Policies>,
}

impl Object for Build {
    const QUERY_CLASS: &'static str = "HarbormasterBuildQuery";
    const TABLE: &'static str = "harbormaster_build";
    const ALIAS: &'static str = "b";
    const ID: Column = Column::new("b.id");

    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            buildable_phid: Phid::new(rows::text(row, "buildablePHID")?),
            plan_phid: Phid::new(rows::text(row, "buildPlanPHID")?),
            status: rows::text(row, "buildStatus")?,
            generation: rows::int(row, "buildGeneration")?,
            initiator_phid: rows::opt_text(row, "initiatorPHID")?,
            plan_auto_key: rows::opt_text(row, "planAutoKey")?,
            dates: (
                rows::int(row, "dateCreated")?,
                rows::int(row, "dateModified")?,
            ),
            policies: None,
        })
    }

    fn id(&self) -> i64 {
        self.id
    }

    async fn visible(request: &ApiRequest<'_>, rows: Vec<Self>) -> Result<Vec<Self>, MethodError> {
        crate::visibility::visible_builds(request, rows).await
    }
}

/// A `harbormaster_buildplan` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// The name.
    pub name: String,
    /// The status constant.
    pub status: String,
    /// The key of an automatic one, set by an application.
    pub auto_key: Option<String>,
    /// The view policy.
    pub view_policy: String,
    /// The edit policy.
    pub edit_policy: String,
    /// The properties, like the plan behaviors.
    pub properties: Map<String, Value>,
    /// `dateCreated` and `dateModified`.
    pub dates: (i64, i64),
}

impl Plan {
    /// `getPolicy()`: autoplans (builtin plans of applications) are
    /// visible to everyone and editable by no one.
    pub(crate) fn policies(&self, most_open: &str) -> Policies {
        if self.auto_key.is_some() {
            Policies {
                view: most_open.to_owned(),
                edit: phab_policy::POLICY_NOONE.to_owned(),
            }
        } else {
            Policies {
                view: self.view_policy.clone(),
                edit: self.edit_policy.clone(),
            }
        }
    }
}

impl Object for Plan {
    const QUERY_CLASS: &'static str = "HarbormasterBuildPlanQuery";
    const TABLE: &'static str = "harbormaster_buildplan";
    const ALIAS: &'static str = "plan";
    const ID: Column = Column::new("plan.id");
    const CURSOR_RAISES_POLICY_EXCEPTIONS: bool = true;

    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        let properties = json_map(row, "properties");
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            name: rows::text(row, "name")?,
            status: rows::text(row, "planStatus")?,
            auto_key: rows::opt_text(row, "planAutoKey")?,
            view_policy: rows::text(row, "viewPolicy")?,
            edit_policy: rows::text(row, "editPolicy")?,
            properties,
            dates: (
                rows::int(row, "dateCreated")?,
                rows::int(row, "dateModified")?,
            ),
        })
    }

    fn id(&self) -> i64 {
        self.id
    }

    async fn visible(request: &ApiRequest<'_>, rows: Vec<Self>) -> Result<Vec<Self>, MethodError> {
        crate::visibility::visible_plans(request, rows).await
    }
}

/// A `harbormaster_buildstep` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// The build plan.
    pub plan_phid: Phid,
    /// The step implementation class.
    pub class_name: String,
    /// The settings of the implementation (`details`).
    pub details: Map<String, Value>,
    /// The name.
    pub name: Option<String>,
    /// The description, in Remarkup.
    pub description: String,
    /// The key of an automatic one, set by an application.
    pub auto_key: Option<String>,
    /// `dateCreated` and `dateModified`.
    pub dates: (i64, i64),
    /// The policies of the plan, once the viewer can see it.
    pub policies: Option<Policies>,
}

impl Object for Step {
    const QUERY_CLASS: &'static str = "HarbormasterBuildStepQuery";
    const TABLE: &'static str = "harbormaster_buildstep";
    const ALIAS: &'static str = "step";
    const ID: Column = Column::new("step.id");

    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            plan_phid: Phid::new(rows::text(row, "buildPlanPHID")?),
            class_name: rows::text(row, "className")?,
            details: json_map(row, "details"),
            name: rows::opt_text(row, "name")?,
            description: rows::text(row, "description")?,
            auto_key: rows::opt_text(row, "stepAutoKey")?,
            dates: (
                rows::int(row, "dateCreated")?,
                rows::int(row, "dateModified")?,
            ),
            policies: None,
        })
    }

    fn id(&self) -> i64 {
        self.id
    }

    async fn visible(request: &ApiRequest<'_>, rows: Vec<Self>) -> Result<Vec<Self>, MethodError> {
        crate::visibility::visible_steps(request, rows).await
    }
}

/// A `harbormaster_buildtarget` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// The build.
    pub build_phid: Phid,
    /// The build step it runs.
    pub step_phid: String,
    /// The step implementation class.
    pub class_name: String,
    /// The settings of the step, when the target was created (`details`).
    pub details: Map<String, Value>,
    /// The build variables (`variables`).
    pub variables: Map<String, Value>,
    /// The name.
    pub name: Option<String>,
    /// The status constant.
    pub status: String,
    /// `dateStarted`.
    pub started: Option<i64>,
    /// `dateCompleted`.
    pub completed: Option<i64>,
    /// `buildGeneration`: how many times it was restarted.
    pub generation: i64,
    /// `dateCreated` and `dateModified`.
    pub dates: (i64, i64),
    /// The policies of the build, once the viewer can see it.
    pub policies: Option<Policies>,
}

impl Object for Target {
    const QUERY_CLASS: &'static str = "HarbormasterBuildTargetQuery";
    const TABLE: &'static str = "harbormaster_buildtarget";
    const ALIAS: &'static str = "target";
    const ID: Column = Column::new("target.id");

    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            build_phid: Phid::new(rows::text(row, "buildPHID")?),
            step_phid: rows::text(row, "buildStepPHID")?,
            class_name: rows::text(row, "className")?,
            details: json_map(row, "details"),
            variables: json_map(row, "variables"),
            name: rows::opt_text(row, "name")?,
            status: rows::text(row, "targetStatus")?,
            started: rows::opt_int(row, "dateStarted")?,
            completed: rows::opt_int(row, "dateCompleted")?,
            generation: rows::int(row, "buildGeneration")?,
            dates: (
                rows::int(row, "dateCreated")?,
                rows::int(row, "dateModified")?,
            ),
            policies: None,
        })
    }

    fn id(&self) -> i64 {
        self.id
    }

    async fn visible(request: &ApiRequest<'_>, rows: Vec<Self>) -> Result<Vec<Self>, MethodError> {
        crate::visibility::visible_targets(request, rows).await
    }
}

/// A `harbormaster_buildartifact` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Artifact {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// The build target.
    pub target_phid: Phid,
    /// `artifactType`, like `uri` or `file`.
    pub kind: String,
    /// `artifactKey`.
    pub key: String,
    /// `artifactData`: the properties of the artifact, like a URI.
    pub data: Map<String, Value>,
    /// `isReleased`.
    pub is_released: bool,
    /// `dateCreated` and `dateModified`.
    pub dates: (i64, i64),
    /// The policies of the target, once the viewer can see it.
    pub policies: Option<Policies>,
}

impl Object for Artifact {
    const QUERY_CLASS: &'static str = "HarbormasterBuildArtifactQuery";
    const TABLE: &'static str = "harbormaster_buildartifact";
    const ALIAS: &'static str = "artifact";
    const ID: Column = Column::new("artifact.id");

    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            target_phid: Phid::new(rows::text(row, "buildTargetPHID")?),
            kind: rows::text(row, "artifactType")?,
            key: rows::text(row, "artifactKey")?,
            data: json_map(row, "artifactData"),
            is_released: rows::flag(row, "isReleased")?,
            dates: (
                rows::int(row, "dateCreated")?,
                rows::int(row, "dateModified")?,
            ),
            policies: None,
        })
    }

    fn id(&self) -> i64 {
        self.id
    }

    async fn visible(request: &ApiRequest<'_>, rows: Vec<Self>) -> Result<Vec<Self>, MethodError> {
        crate::visibility::visible_by_target(
            request,
            rows,
            |artifact| &artifact.target_phid,
            |artifact, policies| {
                artifact.policies = Some(policies);
            },
        )
        .await
    }
}

/// A `harbormaster_buildlog` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Log {
    /// The row ID.
    pub id: i64,
    /// The PHID.
    pub phid: Phid,
    /// The build target.
    pub target_phid: Phid,
    /// `logSource`.
    pub source: Option<String>,
    /// `logType`.
    pub kind: Option<String>,
    /// The size of the log.
    pub byte_length: u64,
    /// The file the log was archived to.
    pub file_phid: Option<String>,
    /// `dateCreated` and `dateModified`.
    pub dates: (i64, i64),
    /// The policies of the target, once the viewer can see it.
    pub policies: Option<Policies>,
}

impl Object for Log {
    const QUERY_CLASS: &'static str = "HarbormasterBuildLogQuery";
    const TABLE: &'static str = "harbormaster_buildlog";
    const ALIAS: &'static str = "log";
    const ID: Column = Column::new("log.id");

    fn from_row(row: &MySqlRow) -> Result<Self, DbError> {
        Ok(Self {
            id: rows::int(row, "id")?,
            phid: Phid::new(rows::text(row, "phid")?),
            target_phid: Phid::new(rows::text(row, "buildTargetPHID")?),
            source: rows::opt_text(row, "logSource")?,
            kind: rows::opt_text(row, "logType")?,
            byte_length: rows::uint(row, "byteLength")?,
            file_phid: rows::opt_text(row, "filePHID")?,
            dates: (
                rows::int(row, "dateCreated")?,
                rows::int(row, "dateModified")?,
            ),
            policies: None,
        })
    }

    fn id(&self) -> i64 {
        self.id
    }

    async fn visible(request: &ApiRequest<'_>, rows: Vec<Self>) -> Result<Vec<Self>, MethodError> {
        crate::visibility::visible_by_target(
            request,
            rows,
            |log| &log.target_phid,
            |log, policies| {
                log.policies = Some(policies);
            },
        )
        .await
    }
}

/// The policies a row inherited, which the visibility check attached.
pub(crate) fn attached(policies: Option<&Policies>) -> Result<&Policies, MethodError> {
    policies.ok_or_else(|| MethodError::internal("a row without its policies"))
}
