//! `HarbormasterTargetWorker`: runs a build target's step, saves the
//! target's new status, then continues its build
//! (`HarbormasterBuildEngine::continueBuild()`).
//!
//! Ported for the steps of this install which Mozilla uses: "Make HTTP
//! Request" (`HarbormasterHTTPRequestBuildStepImplementation`) and the
//! Arcanist lint and unit autotargets, which only wait for their
//! messages. An HTTP step logs and fails without a request when
//! `phabricator.silent` is set or its URI is not one PHP fetches (wrong
//! protocol, a domain which doesn't resolve or resolves to a blacklisted
//! address); a request goes through the [`HttpClient`] the worker is given,
//! and is left to PHP without one. Before the lease, every outcome of the
//! request is planned, so a task is never declined once its request was
//! sent.
//!
//! Left to PHP: the other steps (aborting older builds, waiting for
//! previous builds), HTTP steps with credentials (Passphrase), or whose
//! settings or URI PHP would read differently, and requests the build was
//! restarted during (PHP checks the build every 5 seconds while it waits).

use phab_core::{Config, Phid, PhidType};
use phab_db::Database;
use serde_json::{Map, Value, json};

use super::http::{self, HttpClient, HttpRequest, HttpResponse};
use super::lock::GlobalLock;
use super::log::{self, LOG_WORKER};
use super::sim::{PRIORITY_DEFAULT, Sim, target_is_complete, target_status};
use super::write::{self, Op, TaskData};
use super::{EngineError, Mode, check_config, rows, unsupported};

/// What a target needs from outside to finish: the request of an HTTP
/// step, with the URI its logs are named after.
type Request = (HttpRequest, String);

/// The target's settings (`getSettings()`, its `details`) or variables, as
/// a dictionary.
fn dictionary(raw: &str, what: &str) -> Result<Map<String, Value>, EngineError> {
    match serde_json::from_str(raw) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(Value::Array(items)) if items.is_empty() => Ok(Map::new()),
        _ => Err(unsupported(format!("targets with unusual {what}"))),
    }
}

/// `$target->newLog($source, $type)->append($text)`, closed at once.
fn append_log(sim: &mut Sim, index: usize, (source, kind): (&str, &str), text: &[u8]) {
    let phid = Phid::generate(PhidType::new(*b"HMCL"), None).into_string();
    sim.ops.push(Op::Log {
        phid: phid.clone(),
        target_phid: sim.targets[index].phid.clone(),
        source: source.to_owned(),
        kind: kind.to_owned(),
        log: log::write(text),
    });
    sim.ops.push(Op::Task {
        class: LOG_WORKER,
        data: TaskData::Json(phab_core::php::json_encode(
            &json!({"logPHID": phid, "force": false}),
        )),
        priority: PRIORITY_DEFAULT,
        object_phid: Some(phid),
    });
}

/// `logSilencedCall()`.
fn log_silenced(sim: &mut Sim, index: usize, label: &str) {
    append_log(
        sim,
        index,
        (label, "silenced"),
        b"Declining to make service call because `phabricator.silent` is enabled in \
          configuration.",
    );
}

/// How `execute()` ended.
enum Ended {
    /// It returned: the target passes or waits for messages.
    Returned { wait: bool },
    /// `HarbormasterBuildFailureException`.
    Failed,
    /// It needs the answer to this request.
    Needs(Request),
}

/// `shouldWaitForMessage()` of an HTTP step: the `builtin.wait-for-message`
/// detail `== 'wait'`.
fn waits_for_message(settings: &Map<String, Value>) -> Result<bool, EngineError> {
    match settings.get("builtin.wait-for-message") {
        None | Some(Value::Null | Value::Bool(false)) => Ok(false),
        Some(Value::String(text)) => Ok(text == "wait"),
        Some(Value::Bool(true)) => Ok(true),
        Some(_) => Err(unsupported("HTTP steps with unusual wait settings")),
    }
}

/// `HarbormasterHTTPRequestBuildStepImplementation::execute()`, with the
/// answer to its request once there is one.
async fn execute_http(
    sim: &mut Sim,
    index: usize,
    config: &Config,
    response: Option<&HttpResponse>,
) -> Result<Ended, EngineError> {
    if config.truthy("phabricator.silent") {
        log_silenced(sim, index, "HTTP Request");
        return Ok(Ended::Failed);
    }
    let target = &sim.targets[index];
    let settings = dictionary(&target.details, "settings")?;
    let mut variables = dictionary(&target.variables, "variables")?;
    variables
        .entry("target.phid".to_owned())
        .or_insert_with(|| json!(target.phid));
    let (request, label) = match http::plan(config, &settings, &variables).await? {
        http::Plan::InvalidUri => {
            log_silenced(sim, index, "HTTP Request: Invalid URI");
            return Ok(Ended::Failed);
        }
        http::Plan::Send(request, label) => (request, label),
    };
    let Some(response) = response else {
        return Ok(Ended::Needs((request, label)));
    };
    append_log(
        sim,
        index,
        (&label, "http.head"),
        http::head_log(response).as_bytes(),
    );
    append_log(sim, index, (&label, "http.body"), &response.body);
    if response.is_error() {
        return Ok(Ended::Failed);
    }
    Ok(Ended::Returned {
        wait: waits_for_message(&settings)?,
    })
}

/// `doWork()` up to `continueBuild()`, on the target at `index`.
async fn run_target(
    sim: &mut Sim,
    index: usize,
    config: &Config,
    response: Option<&HttpResponse>,
) -> Result<Option<Request>, EngineError> {
    let now = sim.now;
    let generation = sim.build().generation;
    let target = &mut sim.targets[index];
    if target.started.is_none() {
        target.started = Some(now);
    }
    let ended = if target.generation == generation {
        if target.status == target_status::PENDING {
            target_status::BUILDING.clone_into(&mut target.status);
        }
        match target.class_name.as_str() {
            "HarbormasterArcLintBuildStepImplementation"
            | "HarbormasterArcUnitBuildStepImplementation" => Ended::Returned { wait: true },
            "HarbormasterHTTPRequestBuildStepImplementation" => {
                execute_http(sim, index, config, response).await?
            }
            other => return Err(unsupported(format!("targets of the step {other}"))),
        }
    } else {
        // `HarbormasterBuildAbortedException`: the build restarted.
        let target = &mut sim.targets[index];
        target_status::ABORTED.clone_into(&mut target.status);
        target.completed = Some(now);
        sim.save_target(index)?;
        return Ok(None);
    };
    let target = &mut sim.targets[index];
    match ended {
        Ended::Needs(request) => return Ok(Some(request)),
        Ended::Returned { wait } => {
            let status = if wait {
                target_status::WAITING
            } else {
                target_status::PASSED
            };
            status.clone_into(&mut target.status);
            if target_is_complete(&target.status) {
                target.completed = Some(now);
            }
        }
        Ended::Failed => {
            target_status::FAILED.clone_into(&mut target.status);
            target.completed = Some(now);
        }
    }
    sim.save_target(index)?;
    Ok(None)
}

/// The target's build, loaded as `HarbormasterBuildTargetQuery` attaches
/// it, and where the target is among its targets.
async fn load(database: &Database, id: i64, now: i64) -> Result<(Sim, usize), EngineError> {
    let bad = || EngineError::Permanent(format!("Bad build target ID \"{id}\"."));
    let target = rows::target_by_id(database, id).await?.ok_or_else(bad)?;
    let build = rows::builds_by_phid(database, std::slice::from_ref(&target.build_phid))
        .await?
        .into_iter()
        .next()
        .ok_or_else(bad)?;
    let sim = Sim::load_build(database, &build, now)
        .await?
        .ok_or_else(bad)?;
    let index = sim
        .targets
        .iter()
        .position(|row| row.id == id)
        .ok_or_else(bad)?;
    Ok((sim, index))
}

/// Answers which stand for every outcome of a request, to plan them all.
fn outcomes() -> [HttpResponse; 2] {
    let answer = |status| HttpResponse {
        status,
        transport_error: false,
        headers: Vec::new(),
        body: Vec::new(),
    };
    [answer(200), answer(500)]
}

/// `HarbormasterTargetWorker::doWork()` for the task data `data`, at
/// `now`; requests go through `client`.
pub async fn target_worker(
    database: &Database,
    config: &Config,
    data: &Value,
    now: i64,
    client: Option<&dyn HttpClient>,
    mode: Mode,
) -> Result<(), EngineError> {
    check_config(config)?;
    let Value::Object(map) = data else {
        return Err(unsupported("target tasks whose data is not a dictionary"));
    };
    let (id, _) = rows::id_param(map.get("targetID"))?;
    let (sim, index) = load(database, id, now).await?;
    let mut planned = sim.clone();
    let request = run_target(&mut planned, index, config, None).await?;
    if mode == Mode::Inspect {
        if request.is_none() {
            return planned.continue_build();
        }
        if client.is_none() {
            return Err(unsupported("HTTP requests without an HTTP client"));
        }
        for response in outcomes() {
            let mut outcome = sim.clone();
            run_target(&mut outcome, index, config, Some(&response)).await?;
            outcome.continue_build()?;
        }
        return Ok(());
    }
    let response = match request {
        None => None,
        Some((request, _)) => {
            let client =
                client.ok_or_else(|| unsupported("HTTP requests without an HTTP client"))?;
            Some(client.send(&request).await)
        }
    };
    let build_id = sim.build().id;
    let build_lock = GlobalLock::lock(database, &format!("harbormaster.build:{build_id}")).await?;
    let buildable_lock = match GlobalLock::lock(
        database,
        &format!("harbormaster.buildable:{}", sim.buildable.id),
    )
    .await
    {
        Ok(lock) => lock,
        Err(error) => {
            build_lock.unlock().await?;
            return Err(error);
        }
    };
    let result = async {
        let (mut sim, index) = load(database, id, now).await?;
        if run_target(&mut sim, index, config, response.as_ref())
            .await?
            .is_some()
        {
            return Err(EngineError::Failed(
                "the target asked for a second request".to_owned(),
            ));
        }
        sim.continue_build()?;
        write::apply(database, &sim.ops, now).await
    }
    .await;
    buildable_lock.unlock().await?;
    build_lock.unlock().await?;
    result
}
