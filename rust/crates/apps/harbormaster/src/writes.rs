//! What the write methods (`harbormaster.sendmessage` and
//! `harbormaster.createartifact`) share: the acting user, the build target
//! a call writes to and the run capability on its build plan, the strings
//! Lisk stores, and the `HarbormasterBuildWorker` task.

use phab_auth::Writes;
use phab_conduit::search::{self, Clauses};
use phab_conduit::{ApiRequest, MethodError, pager};
use phab_core::{Phid, php};
use phab_db::sqlx::MySqlConnection;
use phab_db::worker;
use serde_json::{Value, json};

use crate::query::{Order, Query};
use crate::storage::{Build, Plan, Target};
use crate::web;

/// The class of the task which processes a build's messages.
const BUILD_WORKER: &str = "HarbormasterBuildWorker";

pub(crate) fn unsupported(what: &'static str) -> MethodError {
    MethodError::Unsupported(what)
}

/// The acting user of a call which writes. In shadow mode and on
/// read-only installs writes are skipped, so the call is PHP's alone.
pub(crate) fn actor(request: &ApiRequest<'_>) -> Result<String, MethodError> {
    if request.writes != Writes::Allowed {
        return Err(unsupported("writes when they are skipped"));
    }
    // `initializeNewMessage()` falls back on the application's PHID.
    request
        .viewer
        .phid
        .as_ref()
        .map(|phid| phid.as_str().to_owned())
        .ok_or_else(|| unsupported("writes of the omnipotent user"))
}

/// A string parameter (`getValue()`): `None` when it is missing or null.
/// PHP casts other scalars where Rust would have to guess.
pub(crate) fn string_param<'r>(
    request: &'r ApiRequest<'_>,
    key: &str,
) -> Result<Option<&'r str>, MethodError> {
    match request.value(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(unsupported("parameters which are not strings")),
    }
}

/// A string Lisk can store in a `varchar(length)` column.
pub(crate) fn fits(text: &str, length: usize) -> bool {
    text.chars().count() <= length
}

/// The rows of `T` whose `column` is `value` the viewer can see, newest
/// first.
pub(crate) async fn visible_where<T: crate::storage::Object>(
    request: &ApiRequest<'_>,
    column: &str,
    value: &str,
) -> Result<Vec<T>, MethodError> {
    let mut clauses = Clauses::default();
    clauses.in_list(
        &format!("{}.{column}", T::ALIAS),
        search::strings(&[value.to_owned()]),
    );
    Query::new(request, clauses, Order::Newest).all().await
}

/// The build of a target the viewer sees, with its plan if the viewer
/// sees it too (`HarbormasterBuildQuery::didFilterPage()`).
pub(crate) async fn build_and_plan(
    request: &ApiRequest<'_>,
    build_phid: &Phid,
) -> Result<(Build, Option<Plan>), MethodError> {
    let build = web::builds_by_phid(request, std::slice::from_ref(build_phid))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| MethodError::internal("a visible target without its build"))?;
    let plan = plan_of(request, &build).await?;
    Ok((build, plan))
}

/// The plan of a build, if the viewer sees it.
pub(crate) async fn plan_of(
    request: &ApiRequest<'_>,
    build: &Build,
) -> Result<Option<Plan>, MethodError> {
    Ok(
        web::plans_by_phid(request, std::slice::from_ref(&build.plan_phid))
            .await?
            .remove(&build.plan_phid),
    )
}

/// What a build target is written to by: the target, which the viewer
/// sees, and its build.
pub(crate) struct Receiver {
    pub(crate) target: Target,
    pub(crate) build: Build,
}

/// The checks `sendToTarget()` and `createartifact` make before writing to
/// a target: the build plan must be visible, and the viewer must be the
/// initiator of autoplan builds and able to run other plans (T9614).
/// `what` is how the messages name the action, like "send a message to"
/// or "create an artifact for"; `results` how they name what is written.
pub(crate) async fn check_target(
    request: &ApiRequest<'_>,
    target: Target,
    (what, results): (&str, &str),
) -> Result<Receiver, MethodError> {
    let (build, plan) = build_and_plan(request, &target.build_phid).await?;
    let Some(plan) = plan else {
        return Err(pager::core_error(format!(
            "You can not {what} this build target because the build plan could not be \
             loaded."
        )));
    };
    if plan.auto_key.is_some() {
        let Some(initiator) = build
            .initiator_phid
            .as_deref()
            .filter(|phid| php::is_truthy_str(phid))
        else {
            return Err(pager::core_error(format!(
                "You can not {what} this build target. This autoplan build has no recorded \
                 initiator."
            )));
        };
        if request.viewer.phid.as_ref().map(Phid::as_str) != Some(initiator) {
            return Err(pager::core_error(format!(
                "You can not {what} this build target. Only the user who initiated the build \
                 may {results} for autoplan build targets."
            )));
        }
    } else {
        check_run_capability(request, &plan).await?;
    }
    Ok(Receiver { target, build })
}

/// `HarbormasterBuildPlan::assertHasRunCapability()`: `CAN_VIEW` for plans
/// runnable when viewable (which the viewer sees already), `CAN_EDIT`
/// otherwise.
async fn check_run_capability(request: &ApiRequest<'_>, plan: &Plan) -> Result<(), MethodError> {
    if crate::plan::behavior(plan, "runnable")? == "view" {
        return Ok(());
    }
    let can_edit = web::can_edit_plans(request, std::slice::from_ref(plan)).await?;
    if can_edit.first().copied().unwrap_or(false) {
        return Ok(());
    }
    Err(edit_rejection(request, plan)?)
}

/// The policy exception `PhabricatorPolicyFilter::rejectObject()` raises
/// when the viewer, who sees the plan, can't edit it. Only the global
/// policies are explained without loading other objects.
fn edit_rejection(request: &ApiRequest<'_>, plan: &Plan) -> Result<MethodError, MethodError> {
    let config = &request.context.config;
    let policy = plan.policies(crate::most_open(config)).edit;
    // `getOpaquePolicyExplanation()`.
    let explanation = match policy.as_str() {
        phab_policy::POLICY_PUBLIC => {
            "This object is public and can be viewed by anyone, even if they do not have an \
             account on this server."
        }
        phab_policy::POLICY_USER => "Logged in users can take this action.",
        phab_policy::POLICY_ADMIN => "Administrators can take this action.",
        phab_policy::POLICY_NOONE => "By default, no one can take this action.",
        _ => return Err(unsupported("plans editable by other policies")),
    };
    let denied = if config.truthy("phabricator.serious-business") {
        "Access Denied"
    } else {
        "You Shall Not Pass"
    };
    // The handle's name, and the special rules of
    // `HarbormasterBuildPlanPolicyCodex`, which apply to plans not runnable
    // when viewable.
    Ok(pager::core_error(format!(
        "[{denied}: Plan {} {}] (Can Edit) You do not have permission to edit this object. // \
         {explanation} You must have edit permission on this build plan to pause, abort, \
         resume, or restart it. You must have edit permission on this build plan to run it \
         manually.",
        plan.id,
        web::plan_name(plan),
    )))
}

/// Queues the `HarbormasterBuildWorker` task of `build`, which processes
/// its messages: `containerPHID` is set for commands sent to builds
/// (`HarbormasterBuild::sendMessage()`), not for messages sent to targets.
pub(crate) async fn schedule_build_worker(
    connection: &mut MySqlConnection,
    request: &ApiRequest<'_>,
    build: &Build,
    container: Option<&str>,
) -> Result<(), MethodError> {
    // Lisk reads the ID from MySQL as a string.
    let data = php::json_encode(&json!({"buildID": build.id.to_string()}));
    let task = worker::Task {
        class: BUILD_WORKER,
        data: &data,
        priority: worker::PRIORITY_DEFAULT,
        object_phid: Some(build.phid.as_str()),
    };
    worker::schedule_contained_task(
        connection,
        &request.context.database,
        task,
        container,
        request.now,
    )
    .await?;
    Ok(())
}

/// Inserts a `harbormaster_buildmessage` row
/// (`HarbormasterBuildMessage::initializeNewMessage()`).
pub(crate) async fn insert_message(
    connection: &mut MySqlConnection,
    request: &ApiRequest<'_>,
    (author, receiver, kind): (&str, &str, &str),
) -> Result<(), MethodError> {
    let database = &request.context.database;
    phab_db::sqlx::query(&format!(
        "INSERT INTO {} (authorPHID, receiverPHID, type, isConsumed, dateCreated, \
         dateModified) VALUES (?, ?, ?, 0, ?, ?)",
        database.table("harbormaster", "harbormaster_buildmessage")
    ))
    .bind(author)
    .bind(receiver)
    .bind(kind)
    .bind(request.now)
    .bind(request.now)
    .execute(&mut *connection)
    .await?;
    Ok(())
}
