//! `harbormaster.sendmessage` (`HarbormasterSendMessageConduitAPIMethod`):
//! results sent to a build target, with its lint and unit messages
//! (`HarbormasterBuildLintMessage`, `HarbormasterBuildUnitMessage`), and
//! commands sent to a build or to every build of a buildable. Either way
//! the call stores `harbormaster_buildmessage` rows and queues the
//! `HarbormasterBuildWorker` task which processes them, which stays PHP's.
//!
//! Ported: receivers named by their PHID, every message type, lint and
//! unit results, the deprecated `buildTargetPHID`. Left to PHP: receivers
//! named by monograms or of other types, values PHP casts or Lisk refuses
//! (parameters which are not strings, results which don't fit their
//! columns), lint and unit results which fail PHP's type checks (their
//! messages are `PhutilTypeSpec`'s), restarts of builds of plans
//! restartable only if failed (PHP fails with an error there), and run
//! capability checks on plans editable by policies other than the global
//! ones.

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture, pager};
use phab_core::{Phid, PhidType, hash, php};
use phab_db::sqlx::MySqlConnection;
use serde_json::{Map, Value, json};

use crate::storage::{Build, Buildable, Plan, Target};
use crate::web;
use crate::writes::{self, Receiver, fits, string_param, unsupported};

/// The commands builds accept (`HarbormasterBuildMessageTransaction`).
const COMMANDS: &[&str] = &["abort", "pause", "restart", "resume"];

/// The statuses whose `isComplete` is set.
const COMPLETE: &[&str] = &["passed", "failed", "aborted", "error", "deadlocked"];

/// A lint message (`HarbormasterBuildLintMessage::newFromDictionary()`).
#[derive(Debug, Clone, PartialEq)]
struct Lint {
    path: String,
    line: Option<u32>,
    character: Option<u32>,
    code: String,
    severity: String,
    name: String,
    properties: Map<String, Value>,
}

/// A unit result (`HarbormasterBuildUnitMessage::newFromDictionary()`).
#[derive(Debug, Clone, PartialEq)]
struct Unit {
    engine: String,
    namespace: String,
    name: String,
    result: String,
    duration: f64,
    properties: Map<String, Value>,
}

/// `PhutilTypeSpec::checkMap()` of the keys a spec knows: their values,
/// in the spec's order, `None` for missing optional keys. Any failure
/// raises a `PhutilTypeSpec` exception, left to PHP.
fn select<'v>(
    item: &'v Value,
    keys: &[(&str, bool)],
) -> Result<Vec<Option<&'v Value>>, MethodError> {
    let Value::Object(map) = item else {
        return Err(unsupported("results which are not dictionaries"));
    };
    keys.iter()
        .map(|(key, required)| match map.get(*key) {
            None if *required => Err(unsupported("results without required keys")),
            found => Ok(found),
        })
        .collect()
}

/// A `string` of a spec.
fn text(value: Option<&Value>) -> Result<Option<String>, MethodError> {
    match value {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(unsupported("results whose values have other types")),
    }
}

/// A required `string` of a spec.
fn required_text(value: Option<&Value>) -> Result<String, MethodError> {
    text(value)?.ok_or_else(|| MethodError::internal("a required key went missing"))
}

/// An `optional int` of a spec, which the `uint32` column must hold.
fn small_int(value: Option<&Value>) -> Result<Option<u32>, MethodError> {
    match value {
        None => Ok(None),
        Some(Value::Number(number)) if number.is_i64() => number
            .as_i64()
            .and_then(|number| u32::try_from(number).ok())
            .map(Some)
            .ok_or_else(|| unsupported("lint positions which aren't unsigned integers")),
        Some(_) => Err(unsupported("results whose values have other types")),
    }
}

/// `if (strlen($value))`.
fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.is_empty())
}

/// `if ($value)` of a string.
fn truthy(value: Option<String>) -> Option<String> {
    value.filter(|text| php::is_truthy_str(text))
}

impl Lint {
    fn parse(item: &Value) -> Result<Self, MethodError> {
        let values = select(
            item,
            &[
                ("name", true),
                ("code", true),
                ("severity", true),
                ("path", true),
                ("line", false),
                ("char", false),
                ("description", false),
            ],
        )?;
        let name = required_text(values[0])?;
        let code = required_text(values[1])?;
        let severity = required_text(values[2])?;
        let path = required_text(values[3])?;
        let line = small_int(values[4])?;
        let character = small_int(values[5])?;
        let description = text(values[6])?;
        if !(fits(&name, 255) && fits(&code, 128) && fits(&severity, 32)) {
            return Err(unsupported("lint messages longer than their columns"));
        }
        let mut properties = Map::new();
        if let Some(description) = nonempty(description) {
            properties.insert("description".to_owned(), json!(description));
        }
        Ok(Self {
            path,
            line,
            character,
            code,
            severity,
            name,
            properties,
        })
    }
}

/// A key PHP turns into an integer (`"12"`, `"-3"`), which a
/// `map<string, ...>` refuses.
fn is_integer_key(key: &str) -> bool {
    let digits = key.strip_prefix('-').unwrap_or(key);
    !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'))
        && key != "-0"
        && key.parse::<i64>().is_ok()
}

/// Whether PHP encodes `value` back as serde does: integers only, as
/// floats are printed differently.
fn has_only_integers(value: &Value) -> bool {
    match value {
        Value::Number(number) => number.is_i64(),
        Value::Array(items) => items.iter().all(has_only_integers),
        Value::Object(map) => map.values().all(has_only_integers),
        _ => true,
    }
}

/// `(float)` of a duration, then the string Lisk sends MySQL: PHP prints
/// floats with 14 significant digits.
fn stored_duration(value: Option<&Value>) -> Result<f64, MethodError> {
    let duration = match value {
        None => 0.0,
        Some(Value::Number(number)) => number
            .as_f64()
            .ok_or_else(|| unsupported("unusual durations"))?,
        Some(_) => return Err(unsupported("results whose values have other types")),
    };
    if duration == 0.0 {
        // `-0` is stored as is.
        return Ok(duration);
    }
    format!("{duration:.13e}")
        .parse()
        .map_err(|_| unsupported("unusual durations"))
}

impl Unit {
    fn parse(item: &Value) -> Result<Self, MethodError> {
        let values = select(
            item,
            &[
                ("name", true),
                ("result", true),
                ("namespace", false),
                ("engine", false),
                ("duration", false),
                ("path", false),
                ("coverage", false),
                ("details", false),
                ("format", false),
            ],
        )?;
        let name = required_text(values[0])?;
        let result = required_text(values[1])?;
        let namespace = text(values[2])?.unwrap_or_default();
        let engine = text(values[3])?.unwrap_or_default();
        let duration = stored_duration(values[4])?;
        let path = text(values[5])?;
        let coverage = match values[6] {
            None => None,
            Some(Value::Array(items)) if items.is_empty() => None,
            Some(Value::Object(map)) if !map.keys().any(|key| is_integer_key(key)) => {
                Some(map.clone())
            }
            Some(_) => return Err(unsupported("coverage which is not a map of strings")),
        };
        let details = text(values[7])?;
        let format = text(values[8])?;
        if !(fits(&engine, 255) && fits(&namespace, 255) && fits(&result, 32)) {
            return Err(unsupported("unit results longer than their columns"));
        }
        let mut properties = Map::new();
        if let Some(path) = nonempty(path) {
            properties.insert("path".to_owned(), json!(path));
        }
        if let Some(coverage) = coverage.filter(|map| !map.is_empty()) {
            let coverage = Value::Object(coverage);
            if !has_only_integers(&coverage) {
                return Err(unsupported("coverage with floats"));
            }
            properties.insert("coverage".to_owned(), php::round_trip(&coverage));
        }
        if let Some(details) = truthy(details) {
            properties.insert("details".to_owned(), json!(details));
        }
        if let Some(format) = truthy(format) {
            properties.insert("format".to_owned(), json!(format));
        }
        Ok(Self {
            engine,
            namespace,
            name,
            result,
            duration,
            properties,
        })
    }
}

/// A list of results (`getValue($key, array())`), each read by `parse`.
fn results<T>(
    request: &ApiRequest<'_>,
    key: &str,
    parse: fn(&Value) -> Result<T, MethodError>,
) -> Result<Vec<T>, MethodError> {
    match request.value(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) if request.strictly_typed => items.iter().map(parse).collect(),
        Some(_) => Err(unsupported("results which are not lists")),
    }
}

/// `phutil_json_encode()` of a properties dictionary.
fn encode_properties(properties: &Map<String, Value>) -> String {
    php::json_encode(&php::round_trip(&Value::Object(properties.clone())))
}

/// The rows `sendToTarget()` saves, in one transaction: lint messages,
/// unit results with their names (`HarbormasterString::newIndex()`), then
/// the message, and the task.
async fn write_target(
    request: &ApiRequest<'_>,
    receiver: &Receiver,
    (author, kind): (&str, &str),
    (lints, units): (&[Lint], &[Unit]),
) -> Result<(), MethodError> {
    let database = &request.context.database;
    let now = request.now;
    let target = receiver.target.phid.as_str();
    let mut transaction = database.pool().begin().await?;
    let connection: &mut MySqlConnection = &mut transaction;
    for lint in lints {
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (buildTargetPHID, path, line, characterOffset, code, severity, \
             name, properties, dateCreated, dateModified) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            database.table("harbormaster", "harbormaster_buildlintmessage")
        ))
        .bind(target)
        .bind(&lint.path)
        .bind(lint.line)
        .bind(lint.character)
        .bind(&lint.code)
        .bind(&lint.severity)
        .bind(&lint.name)
        .bind(encode_properties(&lint.properties))
        .bind(now)
        .bind(now)
        .execute(&mut *connection)
        .await?;
    }
    for unit in units {
        let index = hash::digest_for_index(unit.name.as_bytes());
        phab_db::sqlx::query(&format!(
            "INSERT IGNORE INTO {} (stringIndex, stringValue) VALUES (?, ?)",
            database.table("harbormaster", "harbormaster_string")
        ))
        .bind(&index)
        .bind(&unit.name)
        .execute(&mut *connection)
        .await?;
        // New writes leave the `name` column empty (T13088).
        phab_db::sqlx::query(&format!(
            "INSERT INTO {} (buildTargetPHID, engine, namespace, name, nameIndex, result, \
             duration, properties, dateCreated, dateModified) VALUES (?, ?, ?, '', ?, ?, ?, ?, \
             ?, ?)",
            database.table("harbormaster", "harbormaster_buildunitmessage")
        ))
        .bind(target)
        .bind(&unit.engine)
        .bind(&unit.namespace)
        .bind(&index)
        .bind(&unit.result)
        .bind(unit.duration)
        .bind(encode_properties(&unit.properties))
        .bind(now)
        .bind(now)
        .execute(&mut *connection)
        .await?;
    }
    writes::insert_message(connection, request, (author, target, kind)).await?;
    writes::schedule_build_worker(connection, request, &receiver.build, None).await?;
    transaction.commit().await?;
    Ok(())
}

/// `sendToTarget()`: any message type, with lint and unit results.
async fn send_to_target(
    request: &ApiRequest<'_>,
    kind: &str,
    target: Target,
) -> Result<Value, MethodError> {
    let receiver =
        writes::check_target(request, target, ("send a message to", "report results")).await?;
    let lints = results(request, "lint", Lint::parse)?;
    let units = results(request, "unit", Unit::parse)?;
    if !fits(kind, 16) {
        return Err(unsupported("message types longer than their column"));
    }
    let author = writes::actor(request)?;
    write_target(request, &receiver, (&author, kind), (&lints, &units)).await?;
    Ok(Value::Null)
}

/// Why a command can't be applied to a build: a
/// `HarbormasterMessageException`, with its title and its body.
type Refusal = (&'static str, &'static str);

/// What `newCanApplyMessageAssertion()` checks of a build.
struct BuildState<'a> {
    build: &'a Build,
    plan: Option<&'a Plan>,
    /// The status of `getBuildPendingStatusObject()`.
    pending: &'a str,
}

/// `HarbormasterBuildMessageTransaction::assertCanApplyMessage()` of each
/// command; `Err` for checks PHP fails with an error.
fn refusal(command: &str, state: &BuildState<'_>) -> Result<Option<Refusal>, MethodError> {
    let build = state.build;
    let is_autobuild = build.plan_auto_key.is_some();
    let is_complete = COMPLETE.contains(&build.status.as_str());
    let is_paused = build.status == "paused";
    Ok(match command {
        "abort" if is_autobuild => Some((
            "Unable to Abort Build",
            "You can not abort a build that uses an autoplan.",
        )),
        "abort" if is_complete => Some((
            "Unable to Abort Build",
            "You can not abort this biuld because it is already complete.",
        )),
        "pause" if is_autobuild => Some((
            "Unable to Pause Build",
            "You can not pause a build that uses an autoplan.",
        )),
        "pause" if is_paused => Some((
            "Unable to Pause Build",
            "You can not pause this build because it is already paused.",
        )),
        "pause" if is_complete => Some((
            "Unable to Pause Build",
            "You can not pause this build because it has already completed.",
        )),
        "resume" if is_autobuild => Some((
            "Unable to Resume Build",
            "You can not resume a build that uses an autoplan.",
        )),
        "resume" if !is_paused && state.pending != "x-pausing" => Some((
            "Unable to Resume Build",
            "You can not resume this build because it is not paused. You can only resume a \
             paused build.",
        )),
        "restart" if is_autobuild => Some((
            "Can Not Restart Autobuild",
            "This build can not be restarted because it is an automatic build.",
        )),
        "restart" => match state.plan {
            None => Some((
                "No Build Plan Permission",
                "You can not restart this build because you do not have permission to access \
                 the build plan.",
            )),
            Some(plan) => match crate::plan::behavior(plan, "restartable")? {
                "never" => Some((
                    "Build Plan Prevents Restart",
                    "This build can not be restarted because the build plan is configured to \
                     prevent the build from restarting.",
                )),
                // `$this->isFailed()` calls a method the transaction doesn't
                // have.
                "failed" => return Err(unsupported("restarts of plans restartable if failed")),
                _ => None,
            },
        },
        _ => None,
    })
}

/// The plans and pending statuses of builds, for [`refusal()`].
async fn build_states(
    request: &ApiRequest<'_>,
    builds: &[Build],
) -> Result<(Vec<Option<Plan>>, Vec<&'static str>), MethodError> {
    let plan_phids: Vec<Phid> = builds.iter().map(|build| build.plan_phid.clone()).collect();
    let plans = web::plans_by_phid(request, &plan_phids).await?;
    let receivers: Vec<Phid> = builds.iter().map(|build| build.phid.clone()).collect();
    let unconsumed = web::messages(&request.context.database, &receivers, true).await?;
    let mut pending = Vec::new();
    for build in builds {
        let own: Vec<web::Message> = unconsumed
            .iter()
            .filter(|message| message.receiver_phid == build.phid)
            .cloned()
            .collect();
        pending.push(
            web::pending_status(&own).unwrap_or(match build.status.as_str() {
                "x-pausing" => "x-pausing",
                _ => "",
            }),
        );
    }
    Ok((
        builds
            .iter()
            .map(|build| plans.get(&build.plan_phid).cloned())
            .collect(),
        pending,
    ))
}

/// `HarbormasterBuild::sendMessage()` of each build, in one transaction.
async fn send_to_builds(
    request: &ApiRequest<'_>,
    command: &str,
    builds: &[&Build],
) -> Result<(), MethodError> {
    if builds.is_empty() {
        return Ok(());
    }
    let author = writes::actor(request)?;
    let mut transaction = request.context.database.pool().begin().await?;
    let connection: &mut MySqlConnection = &mut transaction;
    for build in builds {
        let receiver = build.phid.as_str();
        writes::insert_message(connection, request, (&author, receiver, command)).await?;
        let container = build.buildable_phid.as_str();
        writes::schedule_build_worker(connection, request, build, Some(container)).await?;
    }
    transaction.commit().await?;
    Ok(())
}

/// `getTransactionObjectForMessageType()`: the command, if it is one.
fn command(kind: &str) -> Result<&'static str, MethodError> {
    COMMANDS
        .iter()
        .find(|command| **command == kind)
        .copied()
        .ok_or_else(|| pager::core_error(format!("Message type \"{kind}\" is not supported.")))
}

/// `sendToBuild()`: a command the build accepts.
async fn send_to_build(
    request: &ApiRequest<'_>,
    kind: &str,
    build: Build,
) -> Result<Value, MethodError> {
    let command = command(kind)?;
    let builds = [build];
    let (plans, pending) = build_states(request, &builds).await?;
    let state = BuildState {
        build: &builds[0],
        plan: plans[0].as_ref(),
        pending: pending[0],
    };
    if let Some((title, body)) = refusal(command, &state)? {
        return Err(pager::core_error(format!("{title}: {body}")));
    }
    send_to_builds(request, command, &[&builds[0]]).await?;
    Ok(Value::Null)
}

/// `sendToBuildable()`: the command, to each build of the buildable which
/// accepts it.
async fn send_to_buildable(
    request: &ApiRequest<'_>,
    kind: &str,
    buildable: &Buildable,
) -> Result<Value, MethodError> {
    let command = command(kind)?;
    let builds = web::builds_of_buildables(request, std::slice::from_ref(&buildable.phid)).await?;
    let (plans, pending) = build_states(request, &builds).await?;
    let mut accepting = Vec::new();
    for ((build, plan), pending) in builds.iter().zip(&plans).zip(pending) {
        let state = BuildState {
            build,
            plan: plan.as_ref(),
            pending,
        };
        if refusal(command, &state)?.is_none() {
            accepting.push(build);
        }
    }
    send_to_builds(request, command, &accepting).await?;
    Ok(Value::Null)
}

/// `harbormaster.sendmessage`.
pub struct SendMessage;

impl SendMessage {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let mut receiver = string_param(request, "receiver")?;
        if let Some(legacy) = string_param(request, "buildTargetPHID")? {
            if receiver.is_some() {
                return Err(pager::core_error(
                    "Call specifies both \"receiver\" and \"buildTargetPHID\". When using the \
                     modern \"receiver\" parameter, omit the deprecated \"buildTargetPHID\" \
                     parameter."
                        .to_owned(),
                ));
            }
            receiver = Some(legacy);
        }
        let receiver = receiver.unwrap_or_default();
        if receiver.is_empty() {
            return Err(pager::core_error(
                "Call omits required \"receiver\" parameter. Specify the PHID of the object you \
                 want to send a message to."
                    .to_owned(),
            ));
        }
        let kind = string_param(request, "type")?.unwrap_or_default();
        if kind.is_empty() {
            return Err(pager::core_error(
                "Call omits required \"type\" parameter. Specify the type of message you want \
                 to send."
                    .to_owned(),
            ));
        }
        if !receiver.starts_with("PHID-") {
            return Err(unsupported("receivers named by monograms"));
        }
        let missing = || {
            pager::core_error(format!(
                "Unable to load object \"{receiver}\" to receive message."
            ))
        };
        let phid_type = Phid::new(receiver).phid_type();
        if phid_type == PhidType::new(*b"HMBT") {
            let target = writes::visible_where::<Target>(request, "phid", receiver)
                .await?
                .pop()
                .ok_or_else(missing)?;
            return send_to_target(request, kind, target).await;
        }
        if phid_type != PhidType::new(*b"HMBD") && phid_type != PhidType::new(*b"HMBB") {
            return Err(unsupported("receivers of other types"));
        }
        let loaded = if phid_type == PhidType::new(*b"HMBD") {
            writes::visible_where::<Build>(request, "phid", receiver)
                .await?
                .pop()
                .map(Ok)
        } else {
            writes::visible_where::<Buildable>(request, "phid", receiver)
                .await?
                .pop()
                .map(Err)
        };
        let loaded = loaded.ok_or_else(missing)?;
        for key in ["unit", "lint"] {
            if request.value(key).is_some_and(|value| !value.is_null()) {
                return Err(pager::core_error(format!(
                    "Call includes \"{key}\" parameter. This parameter must be omitted when the \
                     receiver is not a Build Target."
                )));
            }
        }
        match loaded {
            Ok(build) => send_to_build(request, kind, build).await,
            Err(buildable) => send_to_buildable(request, kind, &buildable).await,
        }
    }
}

impl Method for SendMessage {
    fn name(&self) -> &'static str {
        "harbormaster.sendmessage"
    }

    fn params(&self) -> &'static [&'static str] {
        &["receiver", "type", "unit", "lint", "buildTargetPHID"]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_keys_like_php() {
        for key in ["0", "12", "-3"] {
            assert!(is_integer_key(key), "{key}");
        }
        for key in ["", "012", "-0", "1.5", "a", "99999999999999999999", " 1"] {
            assert!(!is_integer_key(key), "{key}");
        }
    }

    #[test]
    fn durations_like_php() {
        assert_eq!(stored_duration(None).ok(), Some(0.0));
        assert_eq!(stored_duration(Some(&json!(3))).ok(), Some(3.0));
        assert_eq!(stored_duration(Some(&json!(1.25))).ok(), Some(1.25));
        // `(string)0.12345678901234568` is "0.12345678901235".
        assert_eq!(
            stored_duration(Some(&json!(0.123_456_789_012_345_68))).ok(),
            Some(0.123_456_789_012_35)
        );
        assert!(stored_duration(Some(&json!("1"))).is_err());
    }

    #[test]
    fn reads_lint_like_php() {
        let lint = Lint::parse(&json!({
            "name": "Missing semicolon",
            "code": "clang-format",
            "severity": "error",
            "path": "dom/Harbor.cpp",
            "line": 4,
            "description": "",
            "extra": [1],
        }))
        .ok();
        assert_eq!(
            lint,
            Some(Lint {
                path: "dom/Harbor.cpp".to_owned(),
                line: Some(4),
                character: None,
                code: "clang-format".to_owned(),
                severity: "error".to_owned(),
                name: "Missing semicolon".to_owned(),
                properties: Map::new(),
            })
        );
        for bad in [
            json!({"name": "n", "code": "c", "severity": "s"}),
            json!({"name": "n", "code": "c", "severity": "s", "path": "p", "line": 1.0}),
            json!({"name": "n", "code": "c", "severity": "s", "path": "p", "char": null}),
            json!({"name": "n", "code": "c", "severity": "s", "path": "p", "line": -1}),
            json!(["n"]),
        ] {
            assert!(Lint::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn reads_units_like_php() {
        let unit = Unit::parse(&json!({
            "name": "test_ships",
            "result": "fail",
            "engine": "mochitest",
            "duration": 1.25,
            "path": "",
            "coverage": {"a.cpp": "NCU"},
            "details": "0",
            "format": "remarkup",
        }))
        .ok();
        let mut properties = Map::new();
        properties.insert("coverage".to_owned(), json!({"a.cpp": "NCU"}));
        properties.insert("format".to_owned(), json!("remarkup"));
        assert_eq!(
            unit,
            Some(Unit {
                engine: "mochitest".to_owned(),
                namespace: String::new(),
                name: "test_ships".to_owned(),
                result: "fail".to_owned(),
                duration: 1.25,
                properties,
            })
        );
        for bad in [
            json!({"name": "n"}),
            json!({"name": "n", "result": "pass", "coverage": {"1": "C"}}),
            json!({"name": "n", "result": "pass", "coverage": ["C"]}),
            json!({"name": "n", "result": "pass", "duration": true}),
        ] {
            assert!(Unit::parse(&bad).is_err(), "{bad}");
        }
    }

    fn build(status: &str, auto_key: Option<&str>) -> Build {
        Build {
            id: 1,
            phid: Phid::new("PHID-HMBD-1"),
            buildable_phid: Phid::new("PHID-HMBB-1"),
            plan_phid: Phid::new("PHID-HMCP-1"),
            status: status.to_owned(),
            generation: 0,
            initiator_phid: None,
            plan_auto_key: auto_key.map(str::to_owned),
            dates: (1, 1),
            policies: None,
        }
    }

    fn plan(properties: Value) -> Plan {
        Plan {
            id: 1,
            phid: Phid::new("PHID-HMCP-1"),
            name: "Try".to_owned(),
            status: "active".to_owned(),
            auto_key: None,
            view_policy: "users".to_owned(),
            edit_policy: "admin".to_owned(),
            properties: match properties {
                Value::Object(map) => map,
                _ => Map::new(),
            },
            dates: (1, 1),
        }
    }

    fn titles(build: &Build, plan: Option<&Plan>, pending: &str) -> Vec<Option<&'static str>> {
        let state = BuildState {
            build,
            plan,
            pending,
        };
        COMMANDS
            .iter()
            .map(|command| {
                refusal(command, &state)
                    .ok()
                    .flatten()
                    .map(|(title, _)| title)
            })
            .collect()
    }

    #[test]
    fn refuses_commands_like_php() {
        let try_plan = plan(json!({}));
        let failed = build("failed", None);
        assert_eq!(
            titles(&failed, Some(&try_plan), ""),
            [
                Some("Unable to Abort Build"),
                Some("Unable to Pause Build"),
                None,
                Some("Unable to Resume Build"),
            ]
        );
        let building = build("building", None);
        assert_eq!(
            titles(&building, None, "x-pausing"),
            [None, None, Some("No Build Plan Permission"), None]
        );
        let paused = build("paused", None);
        let never = plan(json!({"behavior.restartable": "never"}));
        assert_eq!(
            titles(&paused, Some(&never), ""),
            [
                None,
                Some("Unable to Pause Build"),
                Some("Build Plan Prevents Restart"),
                None,
            ]
        );
        let auto = build("building", Some("arcanist"));
        assert_eq!(
            titles(&auto, Some(&try_plan), ""),
            [
                Some("Unable to Abort Build"),
                Some("Unable to Pause Build"),
                Some("Can Not Restart Autobuild"),
                Some("Unable to Resume Build"),
            ]
        );
        let if_failed = plan(json!({"behavior.restartable": "failed"}));
        let state = BuildState {
            build: &building,
            plan: Some(&if_failed),
            pending: "",
        };
        assert!(refusal("restart", &state).is_err());
    }
}
